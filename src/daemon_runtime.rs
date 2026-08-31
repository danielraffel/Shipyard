#[cfg(unix)]
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io;
#[cfg(unix)]
use std::io::{BufRead, BufReader, Read, Write};
#[cfg(unix)]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use base64::Engine;
#[cfg(unix)]
use serde_json::Value;

#[cfg(unix)]
use chrono::Utc;

#[cfg(unix)]
use crate::actionable_wake_producer::{ActionableWakeProducer, ActionableWakeProducerStatus};
use crate::config::LoadedConfig;
#[cfg(unix)]
use crate::custody_transport::CustodyTransportRuntime;
use crate::daemon_ipc::read_daemon_status;
#[cfg(unix)]
use crate::daemon_ipc::{IpcServer, IpcState, github_auth_degraded_message};
#[cfg(unix)]
use crate::execution_supervisor::ExecutionSupervisor;
use crate::identity::RuntimeMode;
#[cfg(unix)]
use crate::paths::RuntimePaths;
#[cfg(unix)]
use crate::reconcile::{
    RECONCILE_INTERVAL_SECONDS, ReconcileReport, ReconcileTransition, ReconcileWindow,
    reconcile_active_ship_states,
};
#[cfg(unix)]
use crate::registrar::{Registrar, RegistrarError, WEBHOOK_SCOPE_COMMAND};
#[cfg(unix)]
use crate::shadow_scheduler::{ShadowDaemonLane, ShadowTransitionEvidence};
#[cfg(unix)]
use crate::ship_resume::{AbandonReport, AbandonedShipState, sweep_orphaned_ship_states};
use crate::ship_state::ShipStateStore;
#[cfg(unix)]
use crate::ship_state::{DispatchedRun, ShipState};
#[cfg(unix)]
use crate::transition_projection_runner::TransitionProjectionRuntime;
#[cfg(unix)]
use crate::tunnel::{
    TailscaleFunnelBackend, TunnelSnapshot, TunnelSupervisorHooks, TunnelSupervisorPolicy,
    TunnelSupervisorState, supervise_tunnel,
};
#[cfg(unix)]
use crate::webhook::{decode_webhook_event, is_valid_signature};
#[cfg(unix)]
use crate::work_ledger::WorkLedger;
#[cfg(unix)]
use crate::workstream_continuation_runtime::{
    ContinuationRuntimeStatus, WorkstreamContinuationRuntime,
};

#[cfg(unix)]
const SHIP_STATE_SCAN_INTERVAL: Duration = Duration::from_secs(1);
#[cfg(unix)]
const WEBHOOK_REGISTRATION_RETRY_INTERVAL: Duration = Duration::from_mins(5);

/// Foreground daemon runtime configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonRunConfig {
    /// Runtime mode used to decide production-vs-sandbox side effects.
    pub mode: RuntimeMode,
    /// Global configuration root passed to delayed execution workers.
    pub global_dir: PathBuf,
    /// Root state directory for the selected runtime mode.
    pub state_dir: PathBuf,
    /// Repos that the daemon should advertise in status.
    pub repos: Vec<String>,
}

/// Detached daemon spawn request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnRequest {
    /// Binary to execute for the child daemon process.
    pub binary: PathBuf,
    /// Runtime mode to preserve across the detached spawn.
    pub mode: RuntimeMode,
    /// Explicit global-dir override, primarily for tests.
    pub global_dir_override: Option<PathBuf>,
    /// Explicit state-dir override, primarily for tests.
    pub state_dir_override: Option<PathBuf>,
    /// Root state directory for the selected runtime mode.
    pub state_dir: PathBuf,
    /// Repos that the daemon should advertise in status.
    pub repos: Vec<String>,
}

/// Errors returned by the foreground daemon runtime.
#[derive(Debug)]
pub enum DaemonRunError {
    /// Another daemon is already serving the selected state root.
    AlreadyRunning,
    /// Underlying filesystem or process-management failure.
    Io(std::io::Error),
    /// IPC startup or teardown failure.
    Protocol(String),
}

impl std::fmt::Display for DaemonRunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyRunning => f.write_str("daemon already running"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Protocol(error) => f.write_str(error),
        }
    }
}

impl std::error::Error for DaemonRunError {}

impl From<std::io::Error> for DaemonRunError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// Detached spawn failure, including the daemon log tail when available.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonSpawnFailedError(pub String);

impl std::fmt::Display for DaemonSpawnFailedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DaemonSpawnFailedError {}

/// Resolve the repo list for daemon startup.
///
/// Explicit repos win. Otherwise, derive the set from active ship-state
/// records so the daemon advertises every repo currently represented on
/// this machine.
#[must_use]
pub fn resolve_repos(state_dir: &Path, explicit_repos: &[String]) -> Vec<String> {
    if !explicit_repos.is_empty() {
        return normalize_repos(explicit_repos.to_vec());
    }

    let Ok(store) = ShipStateStore::new(state_dir.join("ship")) else {
        return Vec::new();
    };
    let states = store.list_active();
    normalize_repos(
        states
            .into_iter()
            .map(|state| state.repo)
            .filter(|repo| !repo.is_empty())
            .collect(),
    )
}

/// Run the daemon in the foreground until a stop request arrives.
#[cfg(unix)]
#[allow(clippy::too_many_lines)]
pub fn run_blocking(config: DaemonRunConfig) -> Result<(), DaemonRunError> {
    let daemon_dir = config.state_dir.join("daemon");
    crate::writer_domain_lease::ensure_protected_dir_all(&daemon_dir)?;

    if read_daemon_status(&config.state_dir).is_some() {
        return Err(DaemonRunError::AlreadyRunning);
    }
    if live_daemon_pid(&daemon_dir).is_some() {
        return Err(DaemonRunError::AlreadyRunning);
    }
    cleanup_stale_runtime_files(&daemon_dir)?;

    let _pid_guard = PidFileGuard::acquire(&daemon_dir.join("daemon.pid"))?;
    let running = Arc::new(AtomicBool::new(true));
    let stop_flag = Arc::clone(&running);
    let last_event_at = Arc::new(Mutex::new(None));
    let enable_tunnel = std::env::var("SHIPYARD_ENABLE_TUNNEL").ok();
    let (webhook_tx, webhook_rx) = mpsc::channel::<Value>();
    let tunnel_runtime = start_tunnel_runtime(
        &running,
        &config.state_dir,
        std::env::var("SHIPYARD_DEV_TUNNEL_URL").ok(),
        config.mode,
        enable_tunnel.as_deref(),
        webhook_tx,
    )?;
    let repos = normalize_repos(config.repos);
    let registrar_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registrar = Registrar::new_with_context(config.mode, &config.state_dir, &registrar_cwd);
    let registration = Arc::new(RegistrationState::new(registrar));
    let registration_error = Arc::new(Mutex::new(None::<String>));
    let execution_error = Arc::new(Mutex::new(None::<String>));
    let transition_projection_error = Arc::new(Mutex::new(None::<String>));
    let custody_transport_error = Arc::new(Mutex::new(None::<String>));
    let continuation_status = Arc::new(Mutex::new(ContinuationRuntimeStatus::default()));
    let actionable_producer_status = Arc::new(Mutex::new(ActionableWakeProducerStatus::default()));
    let ship_dir = config.state_dir.join("ship");
    let ship_dir_for_list = ship_dir.clone();
    let (mut canary_job_runtime, canary_job_init_error) =
        match crate::config::LoadedConfig::load_machine_global_from_dir(config.global_dir.clone()) {
            Ok(loaded) => {
                match crate::parallel_proof_canary_job_adapter::DaemonCanaryJobRuntime::from_config(
                    std::env::current_exe()?,
                    config.mode,
                    config.global_dir.clone(),
                    &config.state_dir,
                    &loaded,
                ) {
                    Ok(runtime) => (runtime, None),
                    Err(error) => (None, Some(format!("parallel_proof_canary_job: {error}"))),
                }
            }
            Err(error) => (
                None,
                Some(format!("parallel_proof_canary_job config: {error}")),
            ),
        };
    let mut canary_diagnostic = canary_job_init_error.clone();
    let canary_capabilities = Arc::new(Mutex::new(
        canary_job_runtime.as_ref().map_or_else(Vec::new, |_| {
            vec![crate::parallel_proof_canary_job_adapter::DAEMON_CANARY_JOB_CAPABILITY.to_owned()]
        }),
    ));
    if let Some(error) = canary_job_init_error.as_ref()
        && let Ok(mut last_error) = execution_error.lock()
    {
        *last_error = Some(error.clone());
    }

    let status_provider = daemon_status_provider(
        Arc::clone(&registration),
        repos.clone(),
        Arc::clone(&registration_error),
        Arc::clone(&execution_error),
        Arc::clone(&transition_projection_error),
        Arc::clone(&custody_transport_error),
        Arc::clone(&continuation_status),
        Arc::clone(&actionable_producer_status),
        Arc::clone(&last_event_at),
        Arc::clone(&tunnel_runtime.snapshot),
        Arc::clone(&canary_capabilities),
    );
    let mut server = IpcServer::new(daemon_dir.join("daemon.sock"), status_provider)
        .with_stop_request(move || {
            stop_flag.store(false, Ordering::Release);
        })
        .with_ship_state_list_provider(move || ship_state_values(&ship_dir_for_list));

    server
        .start()
        .map_err(|error| DaemonRunError::Protocol(error.to_string()))?;

    let (reconcile_tx, reconcile_rx) = mpsc::channel::<ReconcileWorkerResult>();
    let mut reconcile_window = ReconcileWindow::default();
    let mut reconcile_in_flight = false;
    let mut next_reconcile_at = Instant::now() + initial_reconcile_delay();
    let mut previous_states = ship_state_map(&ship_dir);
    let mut next_ship_state_scan_at = Instant::now() + SHIP_STATE_SCAN_INTERVAL;
    let mut registration_sync = RegistrationSyncState::default();
    let mut shadow_lane = ShadowDaemonLane::new(
        config.mode,
        config.global_dir.clone(),
        config.state_dir.clone(),
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        Instant::now(),
    );
    let mut execution_supervisor = ExecutionSupervisor::new(
        std::env::current_exe()?,
        config.mode,
        config.global_dir.clone(),
        config.state_dir.clone(),
    );
    let mut continuation_runtime =
        WorkstreamContinuationRuntime::for_daemon(config.mode, config.state_dir.clone());
    let mut actionable_producer = ActionableWakeProducer::new(config.state_dir.clone());
    let mut transition_projection_runtime = TransitionProjectionRuntime::for_daemon(
        config.mode,
        config.global_dir.clone(),
        config.state_dir.clone(),
    );
    let mut custody_transport_runtime = CustodyTransportRuntime::for_daemon(
        config.mode,
        config.global_dir.clone(),
        config.state_dir.clone(),
    );
    if let Ok(mut published) = transition_projection_error.lock() {
        *published = transition_projection_runtime.diagnostic_error();
    }
    let configured_policy_repositories = WorkLedger::open_existing(&config.state_dir)
        .and_then(|ledger| ledger.map_or_else(|| Ok(Vec::new()), |ledger| ledger.repo_policies()))
        .unwrap_or_default()
        .into_iter()
        .map(|policy| policy.repo)
        .collect::<BTreeSet<_>>();
    for repository in &repos {
        if configured_policy_repositories.contains(repository) {
            actionable_producer.mark_ready(repository, 0, "");
        } else {
            actionable_producer.mark_disabled(repository, "repo_policy_absent");
        }
    }
    let steward_paths = RuntimePaths::current_with_overrides(
        config.mode,
        Some(config.global_dir.clone()),
        Some(config.state_dir.clone()),
    );
    let steward_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut pending_steward_targets = native_steward_inventory(&config.state_dir);
    let (steward_tx, steward_rx) = mpsc::channel::<DaemonStewardResult>();
    let mut steward_in_flight = false;
    if let Ok(mut published) = custody_transport_error.lock() {
        *published = custody_transport_runtime.diagnostic_error();
    }
    if let Ok(mut published) = actionable_producer_status.lock() {
        *published = actionable_producer.status();
    }
    while running.load(Ordering::Acquire) {
        let supervisor_error = execution_supervisor
            .tick()
            .err()
            .map(|error| format!("execution_supervisor: {error}"));
        if let Some(runtime) = canary_job_runtime.as_mut() {
            let result =
                runtime.tick(u64::try_from(Utc::now().timestamp_millis()).unwrap_or(u64::MAX));
            if let Ok(mut capabilities) = canary_capabilities.lock() {
                update_canary_runtime_health(result, &mut capabilities, &mut canary_diagnostic);
            }
        }
        if let Ok(mut last_error) = execution_error.lock() {
            *last_error = supervisor_error
                .or_else(|| canary_diagnostic.clone())
                .or_else(|| canary_job_init_error.clone());
        }
        drain_webhook_events(
            &webhook_rx,
            &server,
            &last_event_at,
            &ship_dir,
            &mut previous_states,
            &mut shadow_lane,
        );
        sync_tunnel_registration(
            &tunnel_runtime,
            &registration,
            &registration_error,
            &repos,
            &mut registration_sync,
            Instant::now(),
        );

        while let Ok(result) = reconcile_rx.try_recv() {
            reconcile_in_flight = false;
            reconcile_window = result.window;
            publish_reconcile_events(&server, &last_event_at, &result.report);
            publish_abandon_events(&server, &last_event_at, &result.abandon);
        }
        while let Ok(completed) = steward_rx.try_recv() {
            steward_in_flight = false;
            let result_identity_valid = completed.result.is_ok()
                && WorkLedger::open_existing(&config.state_dir)
                    .and_then(|ledger| {
                        ledger.map_or_else(
                            || {
                                Err(crate::work_ledger::WorkLedgerError::Refused(
                                    "native work ledger is unavailable".to_owned(),
                                ))
                            },
                            |_| {
                                crate::work_ledger::verify_native_policy_binding_for_repository(
                                    &config.state_dir,
                                    &completed.repository_provider,
                                    &completed.repository_id,
                                    &completed.repository,
                                    completed.pull_request,
                                    &completed.head_sha,
                                )
                            },
                        )
                    })
                    .is_ok();
            if result_identity_valid {
                actionable_producer.mark_ready(
                    &completed.repository,
                    completed.pull_request,
                    &completed.head_sha,
                );
            } else {
                actionable_producer.mark_uncertain(
                    &completed.repository,
                    completed.pull_request,
                    &completed.head_sha,
                );
            }
        }

        let shadow_transitions = shadow_lane.tick(Instant::now());
        for observation in shadow_lane.take_completed_observations() {
            pending_steward_targets.insert((
                observation.repository_provider,
                observation.repository_id,
                observation.repo,
                observation.pr,
                observation.expected_head_sha,
            ));
        }
        if !steward_in_flight
            && let Some((repository_provider, repository_id, repository, pull_request, head_sha)) =
                pending_steward_targets.pop_first()
        {
            let (Some(repository_provider), Some(repository_id)) =
                (repository_provider, repository_id)
            else {
                actionable_producer.mark_uncertain(&repository, pull_request, &head_sha);
                continue;
            };
            let status = actionable_producer.mark_in_flight(&repository, pull_request, &head_sha);
            if let Ok(mut published) = actionable_producer_status.lock() {
                *published = status;
            }
            let result = WorkLedger::open_existing(&config.state_dir)
                .and_then(|ledger| {
                    ledger.map_or_else(
                        || Ok(None),
                        |ledger| {
                            ledger.native_steward_base_ref_for_repository(
                                Some(&repository_provider),
                                Some(&repository_id),
                                &repository,
                                pull_request,
                                &head_sha,
                            )
                        },
                    )
                })
                .map_err(|error| error.to_string())
                .and_then(|base_ref| {
                    base_ref.ok_or_else(|| "exact native steward target is unavailable".to_owned())
                });
            if let Ok(base_ref) = result {
                steward_in_flight = true;
                start_daemon_steward_worker(
                    config.mode,
                    steward_paths.clone(),
                    steward_cwd.clone(),
                    repository_provider,
                    repository_id,
                    repository,
                    pull_request,
                    head_sha,
                    base_ref,
                    steward_tx.clone(),
                );
            } else {
                actionable_producer.mark_uncertain(&repository, pull_request, &head_sha);
            }
        }
        for evidence in shadow_transitions {
            if evidence.transition.observation.is_none() {
                let producer_status = actionable_producer.process(&evidence);
                if let Ok(mut published) = actionable_producer_status.lock() {
                    *published = producer_status;
                }
            }
            publish_shadow_transition(&server, &last_event_at, &evidence);
        }
        // Reconstruct durable terminal stewardship before the consumer may
        // claim a persisted wake. This ordering is required on the very first
        // post-restart loop as well as ordinary daemon cycles.
        actionable_producer.reconcile_durable_terminals();
        if let Ok(mut published) = actionable_producer_status.lock() {
            *published = actionable_producer.status();
        }
        custody_transport_runtime.prepare_native_obligations();
        if custody_transport_runtime.permits_local_continuation() {
            continuation_runtime.tick();
        }
        transition_projection_runtime.tick();
        custody_transport_runtime.tick();
        if let Ok(mut published) = custody_transport_error.lock() {
            *published = custody_transport_runtime.diagnostic_error();
        }
        if let Ok(mut published) = transition_projection_error.lock() {
            *published = transition_projection_runtime.diagnostic_error();
        }
        if let Ok(mut published) = continuation_status.lock() {
            *published = continuation_runtime.status();
        }

        let now = Instant::now();
        let has_subscribers = server.subscriber_count() > 0;
        if should_start_reconcile(has_subscribers, reconcile_in_flight, now, next_reconcile_at) {
            reconcile_in_flight = true;
            next_reconcile_at = now + Duration::from_secs(RECONCILE_INTERVAL_SECONDS);
            start_reconcile_worker(
                config.mode,
                config.state_dir.clone(),
                reconcile_window.clone(),
                reconcile_tx.clone(),
            );
        }
        // Note: we deliberately do NOT push `next_reconcile_at` forward while
        // idle. Doing so would force a freshly-attached subscriber to wait up
        // to a full interval before its first reconcile fires, even though
        // state may have drifted during the idle window. Leaving the deadline
        // stale means the next `should_start_reconcile` call after subscribe
        // sees `now >= next_reconcile_at` immediately. The GitHub-quota
        // savings still hold because `should_start_reconcile` gates on
        // `has_subscribers`.

        if now >= next_ship_state_scan_at {
            next_ship_state_scan_at = now + SHIP_STATE_SCAN_INTERVAL;
            let current_states = ship_state_map(&ship_dir);
            for event in ship_state_delta_events(&previous_states, &current_states) {
                let timestamp = daemon_timestamp();
                if let Ok(mut last_event_at) = last_event_at.lock() {
                    *last_event_at = Some(timestamp);
                }
                server.broadcast_event(event);
            }
            previous_states = current_states;
        }
        thread::sleep(Duration::from_millis(100));
    }

    unregister_webhooks(&registration);
    tunnel_runtime.stop();
    server
        .stop()
        .map_err(|error| DaemonRunError::Protocol(error.to_string()))?;
    Ok(())
}

#[cfg(unix)]
fn update_canary_runtime_health(
    result: Result<crate::parallel_proof_canary_job_adapter::DaemonCanaryTickReport, String>,
    capabilities: &mut Vec<String>,
    diagnostic: &mut Option<String>,
) {
    match result {
        Ok(report) if report.ran => {
            *diagnostic = report
                .warning
                .map(|warning| format!("parallel_proof_canary_job warning: {warning}"));
            *capabilities = vec![
                crate::parallel_proof_canary_job_adapter::DAEMON_CANARY_JOB_CAPABILITY.to_owned(),
            ];
        }
        Ok(_) => {}
        Err(error) => {
            *diagnostic = Some(format!("parallel_proof_canary_job: {error}"));
            capabilities.clear();
        }
    }
}

#[cfg(unix)]
#[allow(clippy::type_complexity)] // Preserve the complete durable repository target key.
fn native_steward_inventory(
    state_dir: &Path,
) -> BTreeSet<(Option<String>, Option<String>, String, u64, String)> {
    WorkLedger::open_existing(state_dir)
        .and_then(|ledger| {
            ledger.map_or_else(|| Ok(Vec::new()), |ledger| ledger.native_steward_targets())
        })
        .unwrap_or_default()
        .into_iter()
        .collect()
}

#[cfg(unix)]
struct DaemonStewardResult {
    repository_provider: String,
    repository_id: String,
    repository: String,
    pull_request: u64,
    head_sha: String,
    result: Result<(), String>,
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
fn start_daemon_steward_worker(
    mode: RuntimeMode,
    runtime_paths: RuntimePaths,
    cwd: PathBuf,
    repository_provider: String,
    repository_id: String,
    repository: String,
    pull_request: u64,
    head_sha: String,
    base_ref: String,
    sender: mpsc::Sender<DaemonStewardResult>,
) {
    thread::spawn(move || {
        let result = crate::app::daemon_steward_repository(
            mode,
            &runtime_paths,
            &cwd,
            &repository_provider,
            &repository_id,
            &repository,
            &base_ref,
        );
        let _ = sender.send(DaemonStewardResult {
            repository_provider,
            repository_id,
            repository,
            pull_request,
            head_sha,
            result,
        });
    });
}

#[cfg(unix)]
struct RegistrationState {
    registrar: Mutex<Registrar>,
    // Status must never wait behind synchronous GitHub webhook I/O. Publish a
    // separate snapshot and refresh it only after registrar mutations finish.
    published_repos: Mutex<Vec<String>>,
}

#[cfg(unix)]
impl RegistrationState {
    fn new(registrar: Registrar) -> Self {
        let published_repos = registrar.all().keys().cloned().collect();
        Self {
            registrar: Mutex::new(registrar),
            published_repos: Mutex::new(published_repos),
        }
    }

    fn published_repos(&self) -> Vec<String> {
        self.published_repos
            .lock()
            .map_or_else(|_| Vec::new(), |repos| repos.clone())
    }

    fn publish(&self, repos: Vec<String>) {
        if let Ok(mut published) = self.published_repos.lock() {
            *published = repos;
        }
    }
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)] // Independent snapshots avoid status blocking on live I/O.
fn daemon_status_provider(
    registration: Arc<RegistrationState>,
    configured_repos: Vec<String>,
    registration_error: Arc<Mutex<Option<String>>>,
    execution_error: Arc<Mutex<Option<String>>>,
    transition_projection_error: Arc<Mutex<Option<String>>>,
    custody_transport_error: Arc<Mutex<Option<String>>>,
    continuation_status: Arc<Mutex<ContinuationRuntimeStatus>>,
    actionable_producer_status: Arc<Mutex<ActionableWakeProducerStatus>>,
    last_event_at: Arc<Mutex<Option<f64>>>,
    tunnel_snapshot: Arc<Mutex<TunnelSnapshot>>,
    capabilities: Arc<Mutex<Vec<String>>>,
) -> impl Fn() -> IpcState + Send + Sync + 'static {
    move || {
        let tunnel = tunnel_snapshot
            .lock()
            .map_or_else(|_| TunnelSnapshot::inactive(), |guard| guard.clone());
        IpcState {
            tunnel_backend: tunnel.backend,
            tunnel_url: tunnel.url,
            tunnel_verified_at: tunnel.verified_at,
            subscribers: 0,
            last_event_at: last_event_at.lock().ok().and_then(|guard| *guard),
            registered_repos: registration.published_repos(),
            configured_repos: configured_repos.clone(),
            capabilities: capabilities
                .lock()
                .map_or_else(|_| Vec::new(), |guard| guard.clone()),
            rate_limit: None,
            workstream_continuation: continuation_status.lock().map_or_else(
                |_| ContinuationRuntimeStatus::default(),
                |guard| guard.clone(),
            ),
            actionable_wake_producer: actionable_producer_status.lock().map_or_else(
                |_| ActionableWakeProducerStatus::default(),
                |guard| guard.clone(),
            ),
            last_error: registration_error
                .lock()
                .ok()
                .and_then(|guard| guard.clone())
                .or_else(|| execution_error.lock().ok().and_then(|guard| guard.clone()))
                .or_else(|| {
                    transition_projection_error
                        .lock()
                        .ok()
                        .and_then(|guard| guard.clone())
                })
                .or_else(|| {
                    custody_transport_error
                        .lock()
                        .ok()
                        .and_then(|guard| guard.clone())
                }),
        }
    }
}

#[cfg(unix)]
fn drain_webhook_events(
    webhook_rx: &mpsc::Receiver<Value>,
    server: &IpcServer,
    last_event_at: &Arc<Mutex<Option<f64>>>,
    ship_dir: &Path,
    previous_states: &mut BTreeMap<(String, u64), ShipState>,
    shadow_lane: &mut ShadowDaemonLane,
) {
    while let Ok(event) = webhook_rx.try_recv() {
        shadow_lane.note_webhook(&event, Instant::now());
        let archived_event =
            archive_closed_pull_request_ship_state(&event, ship_dir, previous_states);
        publish_daemon_event(server, last_event_at, event);
        if let Some(archived_event) = archived_event {
            publish_daemon_event(server, last_event_at, archived_event);
        }
    }
}

#[cfg(unix)]
fn publish_shadow_transition(
    server: &IpcServer,
    last_event_at: &Arc<Mutex<Option<f64>>>,
    evidence: &ShadowTransitionEvidence,
) {
    let event = serde_json::json!({
        "kind": "shadow_observation_transition",
        "payload": {
            "transition": evidence.transition,
            "trigger": evidence.trigger,
            "api_requests": evidence.api_requests,
            "elapsed_ms": evidence.elapsed_ms,
            "fetch_errors": evidence.fetch_errors,
            "observer_only": {
                "activation_enabled": false,
                "dispatch_enabled": false,
                "model_calls": 0,
            },
        }
    });
    // IPC remains a convenience; the daemon's supervised stderr sink is the
    // retained, subscriber-independent audit trail for transition-only shadow
    // evidence. The payload is redacted before it reaches this boundary.
    eprintln!("{}", serde_json::to_string(&event).unwrap_or_default());
    publish_daemon_event(server, last_event_at, event);
}

#[cfg(unix)]
fn publish_daemon_event(server: &IpcServer, last_event_at: &Arc<Mutex<Option<f64>>>, event: Value) {
    let timestamp = daemon_timestamp();
    if let Ok(mut last_event_at) = last_event_at.lock() {
        *last_event_at = Some(timestamp);
    }
    server.broadcast_event(event);
}

#[cfg(unix)]
fn archive_closed_pull_request_ship_state(
    event: &Value,
    ship_dir: &Path,
    previous_states: &mut BTreeMap<(String, u64), ShipState>,
) -> Option<Value> {
    let payload = event.get("payload")?;
    if event.get("kind").and_then(Value::as_str) != Some("pull_request") {
        return None;
    }

    let pr = payload.get("number").and_then(Value::as_u64)?;
    let repo = payload.get("repo").and_then(Value::as_str)?.to_owned();
    let action = payload
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let state = payload
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let merged = payload
        .get("merged")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let closed_at = payload
        .get("closed_at")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let is_terminal =
        action == "closed" || state == "closed" || merged || !closed_at.trim().is_empty();
    if !is_terminal {
        return None;
    }

    let store = ShipStateStore::new(ship_dir.to_path_buf()).ok()?;
    let lock = store.lock_pr_scoped(&repo, pr).ok()?;
    let current = store.get_locked_scoped(&repo, pr, &lock)?;
    store
        .archive_scoped_locked(&repo, pr, &lock)
        .ok()
        .flatten()?;
    previous_states.remove(&(crate::evidence::canonical_repository(&current.repo), pr));

    let outcome = if merged { "merged" } else { "closed" };
    Some(serde_json::json!({
        "kind": "state-archived",
        "payload": {
            "pr": pr,
            "repo": repo,
            "source": "pull_request.closed",
            "message": format!("PR #{pr} {outcome}; archived local ship state."),
        }
    }))
}

#[cfg(unix)]
fn sync_tunnel_registration(
    tunnel_runtime: &TunnelRuntime,
    registration: &Arc<RegistrationState>,
    registration_error: &Arc<Mutex<Option<String>>>,
    repos: &[String],
    registration_sync: &mut RegistrationSyncState,
    now: Instant,
) {
    let current_registration_url = tunnel_runtime.registration_url();
    if let (Some(url), Some(secret)) = (
        current_registration_url.as_deref(),
        tunnel_runtime.webhook_secret.as_deref(),
    ) {
        if !registration_sync.should_attempt(url, now) {
            return;
        }
        if register_webhooks(registration, registration_error, repos, url, secret) {
            registration_sync.record_success(url);
        } else {
            registration_sync.record_failure(url, now);
        }
    } else if let Ok(mut status_error) = registration_error.lock() {
        *status_error = None;
    }
}

#[cfg(unix)]
#[derive(Debug, Default)]
struct RegistrationSyncState {
    registered_url: Option<String>,
    failed_url: Option<String>,
    next_retry_at: Option<Instant>,
}

#[cfg(unix)]
impl RegistrationSyncState {
    fn should_attempt(&self, url: &str, now: Instant) -> bool {
        if self.registered_url.as_deref() == Some(url) {
            return false;
        }
        if self.failed_url.as_deref() == Some(url)
            && self.next_retry_at.is_some_and(|retry_at| now < retry_at)
        {
            return false;
        }
        true
    }

    fn record_success(&mut self, url: &str) {
        self.registered_url = Some(url.to_owned());
        self.failed_url = None;
        self.next_retry_at = None;
    }

    fn record_failure(&mut self, url: &str, now: Instant) {
        self.failed_url = Some(url.to_owned());
        self.next_retry_at = Some(now + WEBHOOK_REGISTRATION_RETRY_INTERVAL);
    }
}

#[cfg(unix)]
fn should_start_reconcile(
    has_subscribers: bool,
    reconcile_in_flight: bool,
    now: Instant,
    next_reconcile_at: Instant,
) -> bool {
    has_subscribers && !reconcile_in_flight && now >= next_reconcile_at
}

#[cfg(unix)]
struct ReconcileWorkerResult {
    report: ReconcileReport,
    window: ReconcileWindow,
    abandon: AbandonReport,
}

#[cfg(unix)]
fn start_reconcile_worker(
    mode: RuntimeMode,
    state_dir: PathBuf,
    mut window: ReconcileWindow,
    sender: mpsc::Sender<ReconcileWorkerResult>,
) {
    thread::spawn(move || {
        let report = reconcile_active_ship_states(&state_dir, &mut window);
        // Opt-in orphan-abandon sweep, same cadence as reconcile. Config is
        // reloaded each pass so toggling `[ship_state] auto_resume` takes effect
        // without a daemon restart; a load failure or the disabled default both
        // yield an empty report (no queue opened, nothing mutated). The load is
        // scoped to the daemon's own runtime mode so an isolated daemon reads
        // its own config, not the Shipyard-mode overlay.
        let abandon = {
            let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            match LoadedConfig::load_from_cwd(mode, &cwd) {
                Ok(config) => sweep_orphaned_ship_states(&state_dir, &config, Utc::now()),
                Err(_) => AbandonReport::default(),
            }
        };
        let _ = sender.send(ReconcileWorkerResult {
            report,
            window,
            abandon,
        });
    });
}

#[cfg(unix)]
fn publish_reconcile_events(
    server: &IpcServer,
    last_event_at: &Arc<Mutex<Option<f64>>>,
    report: &ReconcileReport,
) {
    for transition in &report.transitions {
        let timestamp = daemon_timestamp();
        if let Ok(mut last_event_at) = last_event_at.lock() {
            *last_event_at = Some(timestamp);
        }
        server.broadcast_event(reconcile_healed_event(transition));
    }
}

#[cfg(unix)]
fn publish_abandon_events(
    server: &IpcServer,
    last_event_at: &Arc<Mutex<Option<f64>>>,
    report: &AbandonReport,
) {
    for entry in &report.abandoned {
        let timestamp = daemon_timestamp();
        if let Ok(mut last_event_at) = last_event_at.lock() {
            *last_event_at = Some(timestamp);
        }
        server.broadcast_event(ship_state_abandoned_event(entry));
    }
}

#[cfg(unix)]
fn ship_state_abandoned_event(entry: &AbandonedShipState) -> Value {
    serde_json::json!({
        "kind": "ship_state_abandoned",
        "payload": {
            "pr": entry.pr,
            "repo": entry.repo,
            "evidence": entry.evidence,
            "stalled_minutes": entry.stalled_minutes,
        }
    })
}

#[cfg(unix)]
fn reconcile_healed_event(transition: &ReconcileTransition) -> Value {
    serde_json::json!({
        "kind": "reconcile_healed",
        "payload": {
            "pr": transition.pr,
            "repo": transition.repo,
            "target": transition.target,
            "from_status": transition.from_status,
            "to_status": transition.to_status,
        }
    })
}

#[cfg(unix)]
fn initial_reconcile_delay() -> Duration {
    if cfg!(test) {
        Duration::from_secs(RECONCILE_INTERVAL_SECONDS)
    } else {
        Duration::ZERO
    }
}

/// Non-Unix builds do not expose the daemon IPC runtime yet.
#[cfg(not(unix))]
pub fn run_blocking(config: DaemonRunConfig) -> Result<(), DaemonRunError> {
    drop(config);
    Err(DaemonRunError::Protocol(
        "daemon runtime is only supported on Unix platforms".to_owned(),
    ))
}

/// Spawn the daemon as a detached child process and verify the IPC socket
/// becomes reachable before reporting success.
pub fn spawn_detached(request: &SpawnRequest) -> Result<u32, DaemonSpawnFailedError> {
    let daemon_dir = request.state_dir.join("daemon");
    crate::writer_domain_lease::ensure_protected_dir_all(&daemon_dir)
        .map_err(|error| io_spawn_error(&error))?;
    let temp_dir = prepare_daemon_temp_dir(&daemon_dir).map_err(|error| io_spawn_error(&error))?;

    if read_daemon_status(&request.state_dir).is_some() {
        return Ok(read_pid_file(&daemon_dir.join("daemon.pid")).unwrap_or(0));
    }
    if let Some(pid) = read_pid_file(&daemon_dir.join("daemon.pid"))
        && pid > 0
        && pid_alive(pid)
    {
        if !process_looks_like_shipyard_daemon(pid) {
            return Err(DaemonSpawnFailedError(format!(
                "refusing to replace live process {pid}: daemon identity could not be confirmed"
            )));
        }
        if !terminate_daemon_pid(pid, Duration::from_secs(3)) {
            return Err(DaemonSpawnFailedError(format!(
                "refusing to replace daemon process {pid}: termination could not be confirmed"
            )));
        }
    }
    cleanup_stale_runtime_files(&daemon_dir).map_err(|error| io_spawn_error(&error))?;

    let log_path = daemon_dir.join("daemon.log");
    let retention_config = request.global_dir_override.clone().map_or_else(
        || LoadedConfig::load_machine_global(request.mode),
        LoadedConfig::load_machine_global_from_dir,
    );
    let retention_policy = retention_config.map_or_else(
        |_| crate::log_retention::LogRetentionPolicy::default(),
        |config| crate::log_retention::LogRetentionPolicy::from_config(&config),
    );
    crate::log_retention::rotate_if_oversize(&log_path, retention_policy)
        .map_err(|error| io_spawn_error(&error))?;
    let writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&log_path)
        .map_err(|error| io_spawn_error(&error))?;
    let stdout = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|error| io_spawn_error(&error))?;
    drop(writer_domain);
    let stderr = stdout.try_clone().map_err(|error| io_spawn_error(&error))?;

    let mut command = Command::new(&request.binary);
    command.env("PATH", crate::paths::unattended_tool_path());
    command.env("TMPDIR", &temp_dir);
    command.arg("--mode").arg(request.mode.as_str());
    if let Some(global_dir) = &request.global_dir_override {
        command.arg("--global-dir").arg(global_dir);
    }
    if let Some(state_dir) = &request.state_dir_override {
        command.arg("--state-dir").arg(state_dir);
    }
    command.arg("daemon").arg("run");
    command.env(
        crate::writer_domain_lease::PROTECTED_STDIO_PATH_ENV,
        &log_path,
    );
    for repo in normalize_repos(request.repos.clone()) {
        command.arg("--repo").arg(repo);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::from(stdout));
    command.stderr(Stdio::from(stderr));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
    }

    let child = command.spawn().map_err(|error| io_spawn_error(&error))?;
    let fallback_pid = child.id();
    drop(child);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if read_daemon_status(&request.state_dir).is_some() {
            let pid = read_pid_file(&daemon_dir.join("daemon.pid")).unwrap_or(fallback_pid);
            thread::sleep(Duration::from_millis(300));
            if pid > 0 && !pid_alive(pid) {
                return Err(DaemonSpawnFailedError(format!(
                    "daemon exited immediately after spawn (pid {pid}). Tail of {}:\n{}",
                    log_path.display(),
                    read_log_tail(&log_path)
                )));
            }
            if read_daemon_status(&request.state_dir).is_some() {
                return Ok(pid);
            }
        }
        thread::sleep(Duration::from_millis(50));
    }

    Err(DaemonSpawnFailedError(format!(
        "daemon exited immediately after spawn. Tail of {}:\n{}",
        log_path.display(),
        read_log_tail(&log_path)
    )))
}

fn prepare_daemon_temp_dir(daemon_dir: &Path) -> io::Result<PathBuf> {
    let temp_dir = daemon_dir.join("tmp");
    crate::writer_domain_lease::ensure_protected_dir_all(&temp_dir)?;
    let metadata = fs::symlink_metadata(&temp_dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "daemon temporary path must be a real directory: {}",
                temp_dir.display()
            ),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&temp_dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(temp_dir)
}

/// Best-effort daemon shutdown via IPC stop request.
#[must_use]
pub fn stop_running(state_dir: &Path) -> bool {
    let daemon_dir = state_dir.join("daemon");
    let socket_path = daemon_dir.join("daemon.sock");
    let pid_path = daemon_dir.join("daemon.pid");
    let was_running = read_daemon_status(state_dir).is_some();
    #[cfg(unix)]
    let pid = read_pid_file(&pid_path);

    if socket_path.exists() {
        let _ = send_stop_request(&socket_path);
    }

    if was_running {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if read_daemon_status(state_dir).is_none() && !socket_path.exists() {
                #[cfg(unix)]
                if pid == Some(std::process::id()) {
                    let _ = remove_protected_file(&pid_path);
                    return true;
                }
                #[cfg(unix)]
                if pid.is_some_and(pid_alive) {
                    thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let _ = remove_protected_file(&pid_path);
                return true;
            }
            thread::sleep(Duration::from_millis(100));
        }
        if read_daemon_status(state_dir).is_none() {
            #[cfg(unix)]
            if pid == Some(std::process::id()) {
                let _ = cleanup_stale_runtime_files(&daemon_dir);
                return true;
            }
            #[cfg(unix)]
            if pid.is_some_and(pid_alive) {
                // Fall through to pid-based termination below. A daemon whose
                // socket disappeared while its process survived is not stopped.
            } else {
                let _ = cleanup_stale_runtime_files(&daemon_dir);
                return true;
            }
            #[cfg(not(unix))]
            {
                let _ = cleanup_stale_runtime_files(&daemon_dir);
                return true;
            }
        }
    }

    #[cfg(unix)]
    if let Some(pid) = pid.filter(|pid| *pid > 0)
        && pid_alive(pid)
        && process_looks_like_shipyard_daemon(pid)
    {
        let stopped = terminate_daemon_pid(pid, Duration::from_secs(3));
        if stopped {
            let _ = cleanup_stale_runtime_files(&daemon_dir);
        }
        return stopped;
    }

    if pid_path.exists() || socket_path.exists() {
        let _ = cleanup_stale_runtime_files(&daemon_dir);
    }
    false
}

#[cfg(unix)]
fn ship_state_values(path: &Path) -> Vec<Value> {
    let Ok(store) = ShipStateStore::new(path.to_path_buf()) else {
        return Vec::new();
    };
    store
        .list_active()
        .into_iter()
        .filter_map(|state| serde_json::to_value(state).ok())
        .collect()
}

#[cfg(unix)]
fn ship_state_map(path: &Path) -> BTreeMap<(String, u64), ShipState> {
    let Ok(store) = ShipStateStore::new(path.to_path_buf()) else {
        return BTreeMap::new();
    };
    store
        .list_active()
        .into_iter()
        .map(|state| {
            (
                (crate::evidence::canonical_repository(&state.repo), state.pr),
                state,
            )
        })
        .collect()
}

#[cfg(unix)]
fn ship_state_delta_events(
    previous: &BTreeMap<(String, u64), ShipState>,
    current: &BTreeMap<(String, u64), ShipState>,
) -> Vec<Value> {
    let mut events = Vec::new();

    for (identity, state) in current {
        let previous_state = previous.get(identity);
        for run in &state.dispatched_runs {
            if run_changed(previous_state, run) {
                events.push(workflow_run_event(state, run));
            }
        }
    }

    events
}

#[cfg(unix)]
fn run_changed(previous_state: Option<&ShipState>, run: &DispatchedRun) -> bool {
    let Some(previous_state) = previous_state else {
        return true;
    };
    let previous_run = previous_state
        .dispatched_runs
        .iter()
        .find(|candidate| candidate.target == run.target && candidate.run_id == run.run_id);
    previous_run != Some(run)
}

#[cfg(unix)]
fn workflow_run_event(state: &ShipState, run: &DispatchedRun) -> Value {
    let normalized_status = workflow_status(&run.status);
    let action = if normalized_status == "completed" {
        "completed"
    } else if normalized_status == "queued" {
        "requested"
    } else {
        "in_progress"
    };

    serde_json::json!({
        "kind": "workflow_run",
        "payload": {
            "action": action,
            "run_id": numeric_or_string(&run.run_id),
            "repo": state.repo,
            "head_branch": state.branch,
            "head_sha": state.head_sha,
            "status": normalized_status,
            "conclusion": workflow_conclusion(&run.status),
            "workflow_name": run.target,
            "html_url": Value::Null,
        }
    })
}

#[cfg(unix)]
fn workflow_status(status: &str) -> &'static str {
    match status.to_ascii_lowercase().as_str() {
        "queued" | "pending" | "requested" => "queued",
        "completed" | "pass" | "success" | "failed" | "failure" | "fail" | "cancelled"
        | "canceled" => "completed",
        _ => "in_progress",
    }
}

#[cfg(unix)]
fn workflow_conclusion(status: &str) -> Option<&'static str> {
    match status.to_ascii_lowercase().as_str() {
        "pass" | "success" => Some("success"),
        "failed" | "failure" | "fail" => Some("failure"),
        "cancelled" | "canceled" => Some("cancelled"),
        _ => None,
    }
}

#[cfg(unix)]
fn numeric_or_string(value: &str) -> Value {
    value
        .parse::<i64>()
        .map_or_else(|_| Value::from(value.to_owned()), Value::from)
}

#[cfg(unix)]
fn daemon_timestamp() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

#[cfg(unix)]
fn daemon_tunnel_config(url_override: Option<String>) -> TunnelSnapshot {
    let url_override = url_override.filter(|url| !url.trim().is_empty());
    if let Some(url) = url_override {
        return TunnelSnapshot::development(url, daemon_timestamp());
    }

    TunnelSnapshot::inactive()
}

#[cfg(unix)]
struct TunnelRuntime {
    snapshot: Arc<Mutex<TunnelSnapshot>>,
    webhook: Option<LocalWebhookListener>,
    webhook_secret: Option<String>,
    supervisor: Option<thread::JoinHandle<()>>,
}

#[cfg(unix)]
impl TunnelRuntime {
    fn inactive(snapshot: Arc<Mutex<TunnelSnapshot>>) -> Self {
        Self {
            snapshot,
            webhook: None,
            webhook_secret: None,
            supervisor: None,
        }
    }

    fn registration_url(&self) -> Option<String> {
        let snapshot = self.snapshot.lock().ok()?.clone();
        public_webhook_url_for_snapshot(&snapshot)
    }

    fn stop(self) {
        if let Some(supervisor) = self.supervisor {
            let _ = supervisor.join();
        }
        if let Some(webhook) = self.webhook {
            webhook.stop();
        }
    }
}

#[cfg(unix)]
struct LocalWebhookListener {
    port: u16,
    join: thread::JoinHandle<()>,
}

#[cfg(unix)]
impl LocalWebhookListener {
    fn stop(self) {
        let _ = self.join.join();
    }
}

#[cfg(unix)]
fn start_tunnel_runtime(
    running: &Arc<AtomicBool>,
    state_dir: &Path,
    url_override: Option<String>,
    mode: RuntimeMode,
    enable_tunnel: Option<&str>,
    event_sender: mpsc::Sender<Value>,
) -> Result<TunnelRuntime, DaemonRunError> {
    let initial = daemon_tunnel_config(url_override);
    let has_dev_override = initial.backend == "development";
    let snapshot = Arc::new(Mutex::new(initial));
    if has_dev_override || !tunnel_enabled(enable_tunnel, mode) {
        return Ok(TunnelRuntime::inactive(snapshot));
    }

    let secret = load_or_create_webhook_secret(state_dir)?;
    let webhook = start_webhook_listener(running, event_sender, secret.clone())?;
    let supervisor =
        spawn_tailscale_tunnel_supervisor(Arc::clone(running), Arc::clone(&snapshot), webhook.port);
    Ok(TunnelRuntime {
        snapshot,
        webhook: Some(webhook),
        webhook_secret: Some(secret),
        supervisor: Some(supervisor),
    })
}

#[cfg(unix)]
fn tunnel_enabled(value: Option<&str>, mode: RuntimeMode) -> bool {
    if cfg!(test) {
        return false;
    }
    value.map_or(mode == RuntimeMode::Shipyard, |value| {
        parse_tunnel_enabled(Some(value))
    })
}

#[cfg(unix)]
fn parse_tunnel_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "yes" | "on"
        )
    })
}

#[cfg(unix)]
const WEBHOOK_BODY_LIMIT: usize = 5 * 1024 * 1024;
#[cfg(unix)]
const DELIVERY_DEDUPE_TTL: Duration = Duration::from_mins(5);

#[cfg(unix)]
fn start_webhook_listener(
    running: &Arc<AtomicBool>,
    event_sender: mpsc::Sender<Value>,
    secret: String,
) -> Result<LocalWebhookListener, DaemonRunError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let running = Arc::clone(running);
    let join = thread::spawn(move || {
        let mut seen_delivery_ids = BTreeMap::new();
        while running.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _addr)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let response = match read_webhook_request(&mut stream) {
                        Ok(request) => handle_webhook_request(
                            &request,
                            &secret,
                            &event_sender,
                            &mut seen_delivery_ids,
                        ),
                        Err(response) => response,
                    };
                    let _ = write_http_response(&mut stream, &response);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break,
            }
        }
    });
    Ok(LocalWebhookListener { port, join })
}

#[cfg(unix)]
struct WebhookRequest {
    method: String,
    path: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[cfg(unix)]
struct HttpResponse {
    status: u16,
    body: &'static str,
}

#[cfg(unix)]
impl HttpResponse {
    const fn ok() -> Self {
        Self {
            status: 200,
            body: "ok\n",
        }
    }

    const fn bad_request() -> Self {
        Self {
            status: 400,
            body: "bad request\n",
        }
    }

    const fn unauthorized() -> Self {
        Self {
            status: 401,
            body: "bad signature\n",
        }
    }

    const fn not_found() -> Self {
        Self {
            status: 404,
            body: "not found\n",
        }
    }

    const fn method_not_allowed() -> Self {
        Self {
            status: 405,
            body: "method not allowed\n",
        }
    }
}

#[cfg(unix)]
fn read_webhook_request(stream: &mut TcpStream) -> Result<WebhookRequest, HttpResponse> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    if reader
        .read_line(&mut request_line)
        .map_err(|_| HttpResponse::bad_request())?
        == 0
    {
        return Err(HttpResponse::bad_request());
    }
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        return Err(HttpResponse::bad_request());
    };
    let Some(path) = parts.next() else {
        return Err(HttpResponse::bad_request());
    };

    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|_| HttpResponse::bad_request())?
            == 0
        {
            return Err(HttpResponse::bad_request());
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpResponse::bad_request());
        };
        headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_owned());
    }

    let length = headers
        .get("content-length")
        .map_or(Ok(0), |value| value.parse::<usize>())
        .map_err(|_| HttpResponse::bad_request())?;
    if length > WEBHOOK_BODY_LIMIT {
        return Err(HttpResponse::bad_request());
    }
    let mut body = vec![0_u8; length];
    if length > 0 {
        reader
            .read_exact(&mut body)
            .map_err(|_| HttpResponse::bad_request())?;
    }

    Ok(WebhookRequest {
        method: method.to_owned(),
        path: path
            .split_once('?')
            .map_or(path, |(path, _)| path)
            .to_owned(),
        headers,
        body,
    })
}

#[cfg(unix)]
fn handle_webhook_request(
    request: &WebhookRequest,
    secret: &str,
    event_sender: &mpsc::Sender<Value>,
    seen_delivery_ids: &mut BTreeMap<String, Instant>,
) -> HttpResponse {
    if request.method != "POST" {
        return HttpResponse::method_not_allowed();
    }
    if request.path != "/webhook" && request.path != "/" {
        return HttpResponse::not_found();
    }
    if !is_valid_signature(
        &request.body,
        secret,
        request
            .headers
            .get("x-hub-signature-256")
            .map(String::as_str),
    ) {
        return HttpResponse::unauthorized();
    }

    if let Some(event) = decode_webhook_event(
        request.headers.get("x-github-event").map(String::as_str),
        &request.body,
    ) && should_accept_delivery(
        request.headers.get("x-github-delivery").map(String::as_str),
        seen_delivery_ids,
    ) {
        let _ = event_sender.send(event.to_wire());
    }
    HttpResponse::ok()
}

#[cfg(unix)]
fn should_accept_delivery(
    delivery_id: Option<&str>,
    seen_delivery_ids: &mut BTreeMap<String, Instant>,
) -> bool {
    let Some(delivery_id) = delivery_id.filter(|value| !value.is_empty()) else {
        return true;
    };
    let now = Instant::now();
    let cutoff = now.checked_sub(DELIVERY_DEDUPE_TTL).unwrap_or(now);
    seen_delivery_ids.retain(|_, seen_at| *seen_at >= cutoff);
    if seen_delivery_ids.contains_key(delivery_id) {
        return false;
    }
    seen_delivery_ids.insert(delivery_id.to_owned(), now);
    true
}

#[cfg(unix)]
fn write_http_response(stream: &mut TcpStream, response: &HttpResponse) -> std::io::Result<()> {
    let status_text = match response.status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response.status,
        status_text,
        response.body.len(),
        response.body,
    )?;
    stream.flush()
}

#[cfg(unix)]
fn load_or_create_webhook_secret(state_dir: &Path) -> Result<String, DaemonRunError> {
    let path = state_dir.join("daemon").join("webhook-secret");
    if let Ok(secret) = fs::read_to_string(&path) {
        let secret = secret.trim().to_owned();
        if !secret.is_empty() {
            return Ok(secret);
        }
    }

    let mut random = [0_u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let secret = base64::engine::general_purpose::STANDARD.encode(random);
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&path)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, &secret)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(secret)
}

#[cfg(unix)]
fn spawn_tailscale_tunnel_supervisor(
    running: Arc<AtomicBool>,
    snapshot: Arc<Mutex<TunnelSnapshot>>,
    local_port: u16,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut backend = TailscaleFunnelBackend::default();
        let policy = TunnelSupervisorPolicy::default();
        let mut state = TunnelSupervisorState::default();
        supervise_tunnel(
            &mut backend,
            local_port,
            &policy,
            &mut state,
            TunnelSupervisorHooks::new(
                || !running.load(Ordering::Acquire),
                |delay| interruptible_sleep(&running, delay),
                daemon_timestamp,
                |current| {
                    if let Ok(mut snapshot) = snapshot.lock() {
                        *snapshot = current;
                    }
                },
            ),
        );
    })
}

#[cfg(unix)]
fn interruptible_sleep(running: &Arc<AtomicBool>, delay: Duration) {
    let deadline = Instant::now() + delay;
    while running.load(Ordering::Acquire) && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(std::cmp::min(remaining, Duration::from_millis(100)));
    }
}

#[cfg(unix)]
fn public_webhook_url_for_snapshot(snapshot: &TunnelSnapshot) -> Option<String> {
    if snapshot.backend == "inactive" || snapshot.backend == "development" {
        return None;
    }
    let url = snapshot.url.as_deref()?.trim_end_matches('/');
    if url.is_empty() {
        None
    } else {
        Some(format!("{url}/webhook"))
    }
}

#[cfg(unix)]
fn register_webhooks(
    registration: &Arc<RegistrationState>,
    registration_error: &Arc<Mutex<Option<String>>>,
    repos: &[String],
    public_url: &str,
    secret: &str,
) -> bool {
    let Ok(mut registrar) = registration.registrar.lock() else {
        return false;
    };
    let mut first_error = None;
    for repo in repos {
        if let Err(error) = registrar.ensure_registered(repo, public_url, secret) {
            let message = registration_error_message(repo, &error);
            let _ = crate::writer_domain_lease::write_stderr(format_args!(
                "shipyard daemon: failed to register webhook for {repo}: {message}"
            ));
            if first_error.is_none() {
                first_error = Some(message);
            }
        }
    }
    let repos_snapshot = registrar.all().keys().cloned().collect();
    drop(registrar);
    registration.publish(repos_snapshot);
    let succeeded = first_error.is_none();
    if let Ok(mut status_error) = registration_error.lock() {
        *status_error = first_error;
    }
    succeeded
}

#[cfg(unix)]
fn registration_error_message(repo: &str, error: &RegistrarError) -> String {
    if error.is_missing_webhook_scope() {
        format!(
            "GitHub webhook management for {repo} needs one-time authorization. Polling continues; live webhooks need: {WEBHOOK_SCOPE_COMMAND}"
        )
    } else if error.is_auth_degraded() {
        // Encode the auth-degraded pause reason on the flat `last_error`
        // channel the menu-bar app decodes (github_auth_degraded: <detail>).
        github_auth_degraded_message(&error.auth_degraded_detail())
    } else {
        error.to_string()
    }
}

#[cfg(unix)]
fn unregister_webhooks(registration: &Arc<RegistrationState>) {
    let Ok(mut registrar) = registration.registrar.lock() else {
        return;
    };
    if let Err(error) = registrar.unregister_all() {
        let _ = crate::writer_domain_lease::write_stderr(format_args!(
            "shipyard daemon: failed to unregister webhooks: {error}"
        ));
    }
}

fn send_stop_request(socket_path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let mut stream = UnixStream::connect(socket_path)?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        stream.write_all(br#"{"type":"stop"}"#)?;
        stream.write_all(b"\n")?;
        stream.flush()
    }

    #[cfg(not(unix))]
    {
        let _ = socket_path;
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "daemon IPC stop isn't supported on this platform yet",
        ))
    }
}

fn cleanup_stale_runtime_files(daemon_dir: &Path) -> std::io::Result<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(daemon_dir)?;
    for name in ["daemon.pid", "daemon.sock"] {
        let path = daemon_dir.join(name);
        if path.exists() || path.is_symlink() {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

fn remove_protected_file(path: &Path) -> std::io::Result<()> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn normalize_repos(mut repos: Vec<String>) -> Vec<String> {
    repos.retain(|repo| !repo.is_empty());
    repos.sort();
    repos.dedup();
    repos
}

fn read_pid_file(path: &Path) -> Option<u32> {
    fs::read_to_string(path).ok()?.trim().parse::<u32>().ok()
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    let signalable = Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success());
    if !signalable {
        return false;
    }
    let Ok(output) = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("stat=")
        .stdin(Stdio::null())
        .output()
    else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    !String::from_utf8_lossy(&output.stdout)
        .trim_start()
        .starts_with('Z')
}

#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(unix)]
fn process_looks_like_shipyard_daemon(pid: u32) -> bool {
    let Ok(output) = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("args=")
        .stdin(Stdio::null())
        .output()
    else {
        return false;
    };
    if !output.status.success() {
        return false;
    }

    let args = String::from_utf8_lossy(&output.stdout).to_ascii_lowercase();
    args.contains("shipyard") && args.contains("daemon") && args.contains("run")
}

#[cfg(not(unix))]
fn process_looks_like_shipyard_daemon(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn signal_pid(pid: u32, signal: &str) -> bool {
    Command::new("kill")
        .arg(signal)
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(unix)]
fn wait_until_pid_stops(pid: u32, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        thread::sleep(Duration::from_millis(100));
    }
    !pid_alive(pid)
}

#[cfg(unix)]
fn live_daemon_pid(daemon_dir: &Path) -> Option<u32> {
    let pid = read_pid_file(&daemon_dir.join("daemon.pid"))?;
    (pid > 0 && pid_alive(pid) && process_looks_like_shipyard_daemon(pid)).then_some(pid)
}

#[cfg(unix)]
fn terminate_daemon_pid(pid: u32, timeout: Duration) -> bool {
    if !pid_alive(pid) {
        return true;
    }
    if signal_pid(pid, "-TERM") && wait_until_pid_stops(pid, timeout) {
        return true;
    }
    if pid_alive(pid) {
        let _ = signal_pid(pid, "-KILL");
        let _ = wait_until_pid_stops(pid, Duration::from_secs(1));
    }
    !pid_alive(pid)
}

#[cfg(not(unix))]
fn terminate_daemon_pid(_pid: u32, _timeout: Duration) -> bool {
    true
}

fn read_log_tail(path: &Path) -> String {
    let Ok(bytes) = fs::read(path) else {
        return String::new();
    };
    let tail = if bytes.len() > 2_048 {
        &bytes[bytes.len() - 2_048..]
    } else {
        &bytes
    };
    String::from_utf8_lossy(tail).into_owned()
}

fn io_spawn_error(error: &std::io::Error) -> DaemonSpawnFailedError {
    DaemonSpawnFailedError(error.to_string())
}

#[cfg(unix)]
struct PidFileGuard {
    path: PathBuf,
}

#[cfg(unix)]
impl PidFileGuard {
    fn acquire(path: &Path) -> Result<Self, DaemonRunError> {
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        writeln!(file, "{}", std::process::id())?;
        Ok(Self {
            path: path.to_path_buf(),
        })
    }
}

#[cfg(unix)]
impl Drop for PidFileGuard {
    fn drop(&mut self) {
        let _ = remove_protected_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::io::{BufRead, BufReader, Read, Write};
    #[cfg(unix)]
    use std::net::{Shutdown, TcpStream};
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;
    #[cfg(unix)]
    use std::process::{Command, ExitCode, Stdio};
    #[cfg(unix)]
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(unix)]
    use std::sync::mpsc;
    #[cfg(unix)]
    use std::sync::{Arc, Mutex};
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    #[cfg(unix)]
    use chrono::Utc;
    #[cfg(unix)]
    use serde_json::Value;

    use super::resolve_repos;
    #[cfg(unix)]
    use super::{
        DaemonRunConfig, DaemonRunError, RegistrationState, RegistrationSyncState, TunnelSnapshot,
        WebhookRequest, archive_closed_pull_request_ship_state, daemon_status_provider,
        daemon_tunnel_config, handle_webhook_request, load_or_create_webhook_secret,
        parse_tunnel_enabled, pid_alive, prepare_daemon_temp_dir,
        process_looks_like_shipyard_daemon, reconcile_healed_event, run_blocking, ship_state_map,
        should_start_reconcile, start_tunnel_runtime, stop_running, update_canary_runtime_health,
    };
    #[cfg(unix)]
    use crate::daemon_ipc::{read_daemon_ship_state_list, read_daemon_status};
    #[cfg(unix)]
    use crate::identity::RuntimeMode;
    #[cfg(unix)]
    use crate::reconcile::ReconcileTransition;
    #[cfg(unix)]
    use crate::registrar::Registrar;
    #[cfg(unix)]
    use crate::ship_state::DispatchedRun;
    use crate::ship_state::{ShipState, ShipStateStore};
    #[cfg(unix)]
    use crate::test_support::PROCESS_TREE_TEST_LOCK;
    #[cfg(unix)]
    use crate::webhook::hmac_sha256_hex;
    #[cfg(unix)]
    use wait_timeout::ChildExt;

    #[cfg(unix)]
    #[test]
    fn daemon_temp_dir_is_real_and_owner_private() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let state = tempfile::tempdir().expect("state dir");
        let daemon_dir = state.path().join("daemon");
        std::fs::create_dir(&daemon_dir).expect("daemon dir");
        let temp_dir = prepare_daemon_temp_dir(&daemon_dir).expect("daemon temp dir");
        let metadata = std::fs::symlink_metadata(&temp_dir).expect("temp metadata");
        assert!(metadata.is_dir());
        assert!(!metadata.file_type().is_symlink());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);

        std::fs::remove_dir(&temp_dir).expect("remove temp dir");
        let outside = state.path().join("outside");
        std::fs::create_dir(&outside).expect("outside dir");
        symlink(&outside, &temp_dir).expect("symlink temp dir");
        let error = prepare_daemon_temp_dir(&daemon_dir).expect_err("symlink rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[cfg(unix)]
    #[test]
    fn registration_error_message_encodes_auth_degraded_pause() {
        use crate::daemon_ipc::GITHUB_AUTH_DEGRADED_PREFIX;
        use crate::registrar::RegistrarError;

        let error = RegistrarError::AuthDegraded {
            action: "create",
            output: "HTTP 401: Bad credentials".to_owned(),
        };
        let message = super::registration_error_message("owner/repo", &error);
        assert!(
            message.starts_with(GITHUB_AUTH_DEGRADED_PREFIX),
            "auth-degraded pause must use the GUI-decoded prefix: {message}"
        );
        assert!(message.to_lowercase().contains("bad credentials"));
    }

    #[cfg(unix)]
    #[test]
    fn registration_error_message_keeps_webhook_scope_hint() {
        use crate::daemon_ipc::GITHUB_AUTH_DEGRADED_PREFIX;
        use crate::registrar::RegistrarError;

        let error = RegistrarError::MissingWebhookScope {
            action: "create",
            output: "missing scope: admin:repo_hook".to_owned(),
        };
        let message = super::registration_error_message("owner/repo", &error);
        assert!(!message.starts_with(GITHUB_AUTH_DEGRADED_PREFIX));
        assert!(message.contains("admin:repo_hook"));
    }

    #[test]
    fn resolve_repos_prefers_explicit_and_deduplicates() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repos = resolve_repos(
            temp.path(),
            &[
                "z/r".to_owned(),
                "a/r".to_owned(),
                "z/r".to_owned(),
                String::new(),
            ],
        );

        assert_eq!(repos, vec!["a/r".to_owned(), "z/r".to_owned()]);
    }

    #[test]
    fn resolve_repos_falls_back_to_ship_state_store() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let state = ShipState::new(151, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        store.save(&state).expect("save");

        let repos = resolve_repos(temp.path(), &[]);

        assert_eq!(repos, vec!["owner/repo".to_owned()]);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_tunnel_config_defaults_to_inactive() {
        let tunnel = daemon_tunnel_config(None);

        assert_eq!(tunnel.backend, "inactive");
        assert_eq!(tunnel.url, None);
        assert_eq!(tunnel.verified_at, None);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_tunnel_config_accepts_explicit_dev_override() {
        let tunnel = daemon_tunnel_config(Some("https://shipyard-dev.local".to_owned()));

        assert_eq!(tunnel.backend, "development");
        assert_eq!(tunnel.url.as_deref(), Some("https://shipyard-dev.local"));
        assert!(tunnel.verified_at.is_some());
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_enable_parser_is_explicit_opt_in() {
        assert!(parse_tunnel_enabled(Some("1")));
        assert!(parse_tunnel_enabled(Some("true")));
        assert!(parse_tunnel_enabled(Some("YES")));
        assert!(!parse_tunnel_enabled(Some("0")));
        assert!(!parse_tunnel_enabled(Some("false")));
        assert!(!parse_tunnel_enabled(None));
    }

    #[cfg(unix)]
    #[test]
    fn registration_sync_suppresses_successful_same_url_after_tunnel_flap() {
        let now = Instant::now();
        let mut state = RegistrationSyncState::default();
        let url = "https://node.tailnet.ts.net/webhook";

        assert!(state.should_attempt(url, now));
        state.record_success(url);

        assert!(!state.should_attempt(url, now + Duration::from_secs(30)));
        assert!(state.should_attempt(
            "https://other.tailnet.ts.net/webhook",
            now + Duration::from_secs(30)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn registration_sync_backs_off_failed_same_url() {
        let now = Instant::now();
        let mut state = RegistrationSyncState::default();
        let url = "https://node.tailnet.ts.net/webhook";

        assert!(state.should_attempt(url, now));
        state.record_failure(url, now);

        assert!(!state.should_attempt(url, now + Duration::from_mins(1)));
        assert!(state.should_attempt(url, now + super::WEBHOOK_REGISTRATION_RETRY_INTERVAL));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_does_not_wait_for_registrar_io_lock() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let registration = Arc::new(RegistrationState::new(Registrar::new(state_dir.path())));
        registration.publish(vec!["owner/repo".to_owned()]);
        let provider = daemon_status_provider(
            Arc::clone(&registration),
            vec!["owner/repo".to_owned(), "owner/pending".to_owned()],
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(
                crate::workstream_continuation_runtime::ContinuationRuntimeStatus::default(),
            )),
            Arc::new(Mutex::new(
                crate::actionable_wake_producer::ActionableWakeProducerStatus::default(),
            )),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(TunnelSnapshot::inactive())),
            Arc::new(Mutex::new(Vec::new())),
        );

        let registrar_io = registration.registrar.lock().expect("registrar lock");
        let (tx, rx) = mpsc::channel();
        let worker = thread::spawn(move || tx.send(provider()).expect("send status"));
        let status = rx.recv_timeout(Duration::from_millis(250));
        drop(registrar_io);
        worker.join().expect("status worker");
        let status = status.expect("status must not wait for registrar network I/O");

        assert_eq!(status.registered_repos, vec!["owner/repo"]);
        assert_eq!(status.configured_repos, vec!["owner/repo", "owner/pending"]);
    }

    #[cfg(unix)]
    #[test]
    fn daemon_status_surfaces_transition_projection_drain_failure() {
        let state_dir = tempfile::tempdir().expect("state dir");
        let registration = Arc::new(RegistrationState::new(Registrar::new(state_dir.path())));
        let projection_error = Arc::new(Mutex::new(Some(
            "transition-projection-intent-drain-state-mutation".to_owned(),
        )));
        let provider = daemon_status_provider(
            registration,
            Vec::new(),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(None)),
            projection_error,
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(
                crate::workstream_continuation_runtime::ContinuationRuntimeStatus::default(),
            )),
            Arc::new(Mutex::new(
                crate::actionable_wake_producer::ActionableWakeProducerStatus::default(),
            )),
            Arc::new(Mutex::new(None)),
            Arc::new(Mutex::new(TunnelSnapshot::inactive())),
            Arc::new(Mutex::new(Vec::new())),
        );

        assert_eq!(
            provider().last_error.as_deref(),
            Some("transition-projection-intent-drain-state-mutation")
        );
    }

    #[cfg(unix)]
    #[test]
    fn canary_capability_recovers_after_a_later_clean_tick() {
        let mut capabilities =
            vec![crate::parallel_proof_canary_job_adapter::DAEMON_CANARY_JOB_CAPABILITY.to_owned()];
        let mut diagnostic = None;
        update_canary_runtime_health(
            Err("transient store failure".to_owned()),
            &mut capabilities,
            &mut diagnostic,
        );
        assert!(capabilities.is_empty());
        assert!(
            diagnostic
                .as_deref()
                .unwrap()
                .contains("transient store failure")
        );

        update_canary_runtime_health(
            Ok(
                crate::parallel_proof_canary_job_adapter::DaemonCanaryTickReport {
                    ran: true,
                    processed_jobs: 1,
                    warning: Some("one corrupt record was isolated".to_owned()),
                },
            ),
            &mut capabilities,
            &mut diagnostic,
        );
        assert_eq!(
            capabilities,
            vec![crate::parallel_proof_canary_job_adapter::DAEMON_CANARY_JOB_CAPABILITY]
        );
        assert!(diagnostic.as_deref().unwrap().contains("corrupt record"));

        update_canary_runtime_health(
            Ok(
                crate::parallel_proof_canary_job_adapter::DaemonCanaryTickReport {
                    ran: true,
                    processed_jobs: 0,
                    warning: None,
                },
            ),
            &mut capabilities,
            &mut diagnostic,
        );
        assert!(diagnostic.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_requires_active_subscriber_and_due_time() {
        let now = Instant::now();

        assert!(!should_start_reconcile(false, false, now, now));
        assert!(!should_start_reconcile(true, true, now, now));
        assert!(!should_start_reconcile(
            true,
            false,
            now,
            now + Duration::from_secs(1)
        ));
        assert!(should_start_reconcile(true, false, now, now));
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_fires_immediately_after_subscriber_attaches() {
        // A stale `next_reconcile_at` (well in the past, from a long idle
        // window) plus a freshly-attached subscriber must reconcile on the
        // *very next* loop iteration — not on the iteration after that.
        // The previous bug pushed `next_reconcile_at = now + interval`
        // every idle tick, so attach → wait up to a full interval.
        let now = Instant::now();
        let stale_deadline = now.checked_sub(Duration::from_mins(10)).unwrap_or(now);
        // No subscribers → gate stays closed (the loop must not have rewritten
        // the deadline forward during idle).
        assert!(!should_start_reconcile(false, false, now, stale_deadline));
        // Subscriber attaches; deadline is still the stale value. Gate fires
        // immediately on this iteration.
        assert!(should_start_reconcile(true, false, now, stale_deadline));
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_runtime_dev_override_does_not_spawn_real_tunnel() {
        let running = Arc::new(AtomicBool::new(true));
        let temp = tempfile::tempdir().expect("tempdir");
        let (tx, _rx) = mpsc::channel();
        let runtime = start_tunnel_runtime(
            &running,
            temp.path(),
            Some("https://shipyard-dev.local".to_owned()),
            RuntimeMode::Isolated,
            Some("1"),
            tx,
        )
        .expect("runtime");
        let snapshot = runtime.snapshot.lock().expect("snapshot").clone();

        assert_eq!(snapshot.backend, "development");
        assert_eq!(snapshot.url.as_deref(), Some("https://shipyard-dev.local"));
        assert!(runtime.supervisor.is_none());
        assert!(runtime.webhook.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn tunnel_runtime_stays_inactive_without_opt_in() {
        let running = Arc::new(AtomicBool::new(true));
        let temp = tempfile::tempdir().expect("tempdir");
        let (tx, _rx) = mpsc::channel();
        let runtime = start_tunnel_runtime(
            &running,
            temp.path(),
            None,
            RuntimeMode::Isolated,
            Some("false"),
            tx,
        )
        .expect("runtime");
        let snapshot = runtime.snapshot.lock().expect("snapshot").clone();

        assert_eq!(snapshot.backend, "inactive");
        assert!(runtime.supervisor.is_none());
        assert!(runtime.webhook.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn webhook_handler_accepts_signed_event_and_dedupes_delivery() {
        let (tx, rx) = mpsc::channel();
        let secret = "dev-secret";
        let body = br#"{
            "action":"completed",
            "repository":{"full_name":"owner/repo"},
            "workflow_run":{
                "id":321,
                "head_branch":"main",
                "head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "status":"completed",
                "conclusion":"success",
                "name":"CI",
                "html_url":"https://github.com/owner/repo/actions/runs/321"
            }
        }"#;
        let mut seen = BTreeMap::new();
        let response = handle_webhook_request(
            &signed_webhook_request(secret, "delivery-1", "workflow_run", body),
            secret,
            &tx,
            &mut seen,
        );

        assert_eq!(response.status, 200);
        let event = rx.try_recv().expect("event");
        assert_eq!(event["kind"], "workflow_run");
        assert_eq!(event["payload"]["repo"], "owner/repo");
        assert_eq!(event["payload"]["run_id"], 321);

        let response = handle_webhook_request(
            &signed_webhook_request(secret, "delivery-1", "workflow_run", body),
            secret,
            &tx,
            &mut seen,
        );

        assert_eq!(response.status, 200);
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn webhook_handler_rejects_wrong_method_path_and_signature() {
        let (tx, rx) = mpsc::channel();
        let secret = "dev-secret";
        let body = br#"{"zen":"Keep it logically awesome."}"#;
        let mut seen = BTreeMap::new();
        let mut request = signed_webhook_request(secret, "delivery-1", "ping", body);
        request.method = "GET".to_owned();

        let response = handle_webhook_request(&request, secret, &tx, &mut seen);
        assert_eq!(response.status, 405);

        let mut request = signed_webhook_request(secret, "delivery-2", "ping", body);
        request.path = "/not-webhook".to_owned();
        let response = handle_webhook_request(&request, secret, &tx, &mut seen);
        assert_eq!(response.status, 404);

        let mut request = signed_webhook_request(secret, "delivery-3", "ping", body);
        request
            .headers
            .insert("x-hub-signature-256".to_owned(), "sha256=bad".to_owned());
        let response = handle_webhook_request(&request, secret, &tx, &mut seen);
        assert_eq!(response.status, 401);
        assert!(rx.try_recv().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn webhook_listener_serves_signed_http_delivery() {
        let running = Arc::new(AtomicBool::new(true));
        let (tx, rx) = mpsc::channel();
        let secret = "dev-secret";
        let listener =
            super::start_webhook_listener(&running, tx, secret.to_owned()).expect("listener");
        let body = br#"{
            "action":"published",
            "repository":{"full_name":"owner/repo"},
            "release":{
                "tag_name":"v1.2.3",
                "draft":false,
                "prerelease":false,
                "assets":[{"name":"shipyard.dmg","state":"uploaded","size":42}]
            }
        }"#;
        let signature = hmac_sha256_hex(body, secret);
        let request = format!(
            "POST /webhook HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nX-Hub-Signature-256: sha256={}\r\nX-GitHub-Event: release\r\nX-GitHub-Delivery: delivery-http-1\r\n\r\n",
            body.len(),
            signature,
        );
        let mut request_bytes = request.into_bytes();
        request_bytes.extend_from_slice(body);
        let mut response = String::new();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let mut stream = TcpStream::connect(("127.0.0.1", listener.port)).expect("connect");
            stream.write_all(&request_bytes).expect("request");
            stream.shutdown(Shutdown::Write).expect("shutdown");
            response.clear();
            match stream.read_to_string(&mut response) {
                Ok(_) if response.starts_with("HTTP/1.1 200 OK") => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
                Err(error) => panic!("response: {error}"),
            }
            thread::sleep(Duration::from_millis(25));
        }

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "unexpected response: {response:?}"
        );
        let event = rx.recv_timeout(Duration::from_secs(1)).expect("event");
        assert_eq!(event["kind"], "release");
        assert_eq!(event["payload"]["tag_name"], "v1.2.3");
        assert_eq!(event["payload"]["assets"][0]["name"], "shipyard.dmg");

        running.store(false, Ordering::Release);
        listener.stop();
    }

    #[cfg(unix)]
    #[test]
    fn webhook_secret_is_stable_and_private() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = load_or_create_webhook_secret(temp.path()).expect("secret");
        let second = load_or_create_webhook_secret(temp.path()).expect("secret");

        assert_eq!(first, second);
        assert!(!first.is_empty());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let mode = std::fs::metadata(temp.path().join("daemon").join("webhook-secret"))
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[cfg(unix)]
    fn signed_webhook_request(
        secret: &str,
        delivery_id: &str,
        event: &str,
        body: &[u8],
    ) -> WebhookRequest {
        let mut headers = BTreeMap::new();
        headers.insert(
            "x-hub-signature-256".to_owned(),
            format!("sha256={}", hmac_sha256_hex(body, secret)),
        );
        headers.insert("x-github-delivery".to_owned(), delivery_id.to_owned());
        headers.insert("x-github-event".to_owned(), event.to_owned());
        WebhookRequest {
            method: "POST".to_owned(),
            path: "/webhook".to_owned(),
            headers,
            body: body.to_vec(),
        }
    }

    #[cfg(unix)]
    fn seed_registered_repos(state_dir: &std::path::Path, repos: &[&str]) {
        let daemon_dir = state_dir.join("daemon");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let payload = repos
            .iter()
            .enumerate()
            .map(|(index, repo)| {
                serde_json::json!({
                    "repo": repo,
                    "hook_id": u64::try_from(index + 1).expect("hook id"),
                })
            })
            .collect::<Vec<_>>();
        std::fs::write(
            daemon_dir.join("registrations.json"),
            serde_json::to_string_pretty(&payload).expect("registrations json"),
        )
        .expect("write registrations");
    }

    #[cfg(unix)]
    #[test]
    fn reconcile_healed_event_matches_python_wire_shape() {
        let event = reconcile_healed_event(&ReconcileTransition {
            pr: 42,
            repo: "owner/repo".to_owned(),
            target: "macos".to_owned(),
            from_status: "failed".to_owned(),
            to_status: "completed".to_owned(),
        });

        assert_eq!(event["kind"], "reconcile_healed");
        assert_eq!(event["payload"]["pr"], 42);
        assert_eq!(event["payload"]["repo"], "owner/repo");
        assert_eq!(event["payload"]["target"], "macos");
        assert_eq!(event["payload"]["from_status"], "failed");
        assert_eq!(event["payload"]["to_status"], "completed");
    }

    #[cfg(unix)]
    #[test]
    fn closed_pull_request_webhook_archives_matching_ship_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ship_dir = temp.path().join("ship");
        let store = ShipStateStore::new(ship_dir.clone()).expect("store");
        let state = ShipState::new(151, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        store.save(&state).expect("save");
        let mut previous_states = ship_state_map(&ship_dir);
        let event = serde_json::json!({
            "kind": "pull_request",
            "payload": {
                "action": "closed",
                "number": 151,
                "repo": "owner/repo",
                "state": "closed",
                "merged": true,
                "closed_at": "2026-05-06T12:00:00Z",
                "merged_at": "2026-05-06T12:00:00Z",
            }
        });

        let archived_event =
            archive_closed_pull_request_ship_state(&event, &ship_dir, &mut previous_states)
                .expect("archive event");

        assert!(store.get(151).is_none());
        assert_eq!(store.list_archived().len(), 1);
        assert!(!previous_states.contains_key(&("owner/repo".to_owned(), 151)));
        assert_eq!(archived_event["kind"], "state-archived");
        assert_eq!(archived_event["payload"]["pr"], 151);
        assert_eq!(archived_event["payload"]["repo"], "owner/repo");
        assert_eq!(archived_event["payload"]["source"], "pull_request.closed");
    }

    #[cfg(unix)]
    #[test]
    fn closed_pull_request_webhook_ignores_same_number_from_other_repo() {
        let temp = tempfile::tempdir().expect("tempdir");
        let ship_dir = temp.path().join("ship");
        let store = ShipStateStore::new(ship_dir.clone()).expect("store");
        store
            .save(&ShipState::new(
                151,
                "owner/repo",
                "feature/x",
                "main",
                "a".repeat(40),
                "p1",
            ))
            .expect("save");
        let mut previous_states = ship_state_map(&ship_dir);
        let event = serde_json::json!({
            "kind": "pull_request",
            "payload": {
                "action": "closed",
                "number": 151,
                "repo": "other/repo",
                "state": "closed",
                "merged": false,
                "closed_at": "2026-05-06T12:00:00Z",
            }
        });

        assert!(
            archive_closed_pull_request_ship_state(&event, &ship_dir, &mut previous_states)
                .is_none()
        );
        assert!(store.get(151).is_some());
        assert!(previous_states.contains_key(&("owner/repo".to_owned(), 151)));
    }

    #[cfg(unix)]
    #[test]
    fn run_blocking_serves_status_and_ship_state_list_until_stopped() {
        let temp = tempfile::tempdir().expect("tempdir");
        seed_registered_repos(temp.path(), &["owner/repo"]);
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let mut state =
            ShipState::new(151, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        state.pr_title = "Test ship".to_owned();
        state.dispatched_runs.push(DispatchedRun {
            target: "macos".to_owned(),
            provider: "local".to_owned(),
            run_id: "42".to_owned(),
            status: "completed".to_owned(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            attempt: 1,
            last_heartbeat_at: None,
            phase: None,
            required: true,
        });
        store.save(&state).expect("save");

        let state_dir = temp.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            run_blocking(DaemonRunConfig {
                mode: RuntimeMode::Isolated,
                global_dir: state_dir.clone(),
                state_dir,
                repos: vec!["owner/repo".to_owned()],
            })
            .expect("daemon runtime");
            ExitCode::SUCCESS
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while read_daemon_status(temp.path()).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let daemon_status = read_daemon_status(temp.path()).expect("status");
        let ship_states = read_daemon_ship_state_list(temp.path()).expect("ship-state list");

        assert_eq!(daemon_status["registered_repos"][0], "owner/repo");
        assert_eq!(ship_states.len(), 1);
        assert_eq!(ship_states[0]["pr"], 151);
        assert_eq!(ship_states[0]["pr_title"], "Test ship");

        assert!(stop_running(temp.path()));
        assert_eq!(worker.join().expect("join"), ExitCode::SUCCESS);
        assert!(read_daemon_status(temp.path()).is_none());
    }

    #[cfg(unix)]
    #[test]
    fn run_blocking_rejects_already_running_state_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_dir = temp.path().to_path_buf();
        let other_state_dir = state_dir.clone();
        let worker = std::thread::spawn(move || {
            run_blocking(DaemonRunConfig {
                mode: RuntimeMode::Isolated,
                global_dir: state_dir.clone(),
                state_dir,
                repos: vec!["owner/repo".to_owned()],
            })
            .expect("daemon runtime");
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while read_daemon_status(temp.path()).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let error = run_blocking(DaemonRunConfig {
            mode: RuntimeMode::Isolated,
            global_dir: other_state_dir.clone(),
            state_dir: other_state_dir,
            repos: vec!["owner/repo".to_owned()],
        })
        .expect_err("already running");
        assert!(matches!(error, DaemonRunError::AlreadyRunning));

        assert!(stop_running(temp.path()));
        worker.join().expect("join");
    }

    #[cfg(unix)]
    #[test]
    fn stop_running_terminates_pid_when_ipc_is_unavailable() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let daemon_dir = temp.path().join("daemon");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let script = temp.path().join("shipyard-daemon-run.sh");
        let pid_path = daemon_dir.join("daemon.pid");
        std::fs::write(
            &script,
            "#!/bin/sh\npid_file=\"$1\"\necho $$ > \"$pid_file\"\ntrap 'rm -f \"$pid_file\"; exit 0' TERM\nwhile true; do sleep 1; done\n",
        )
        .expect("script");
        let mut child = Command::new("sh")
            .arg(&script)
            .arg(&pid_path)
            .arg("daemon")
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon-shaped child");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pid_path.exists(), "pid file was not written");
        let identity_deadline = Instant::now() + Duration::from_secs(1);
        while !process_looks_like_shipyard_daemon(child.id()) && Instant::now() < identity_deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            process_looks_like_shipyard_daemon(child.id()),
            "daemon identity was not observable"
        );

        assert!(stop_running(temp.path()));

        let status = child
            .wait_timeout(Duration::from_secs(1))
            .expect("wait")
            .or_else(|| {
                let _ = child.kill();
                child.wait().ok()
            })
            .expect("child exited");
        assert!(status.success());
        assert!(!pid_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stop_running_kills_daemon_that_removes_pid_file_but_survives_term() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let daemon_dir = temp.path().join("daemon");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let script = temp.path().join("shipyard-daemon-run-stubborn.sh");
        let pid_path = daemon_dir.join("daemon.pid");
        std::fs::write(
            &script,
            "#!/bin/sh\npid_file=\"$1\"\necho $$ > \"$pid_file\"\ntrap 'rm -f \"$pid_file\"' TERM\nwhile true; do sleep 1; done\n",
        )
        .expect("script");
        let mut child = Command::new("sh")
            .arg(&script)
            .arg(&pid_path)
            .arg("daemon")
            .arg("run")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon-shaped child");
        let deadline = Instant::now() + Duration::from_secs(1);
        while !pid_path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(pid_path.exists(), "pid file was not written");

        assert!(stop_running(temp.path()));

        let status = child
            .wait_timeout(Duration::from_secs(2))
            .expect("wait")
            .or_else(|| {
                let _ = child.kill();
                child.wait().ok()
            })
            .expect("child exited");
        assert!(!status.success(), "stubborn daemon should require SIGKILL");
        assert!(!pid_alive(child.id()));
        assert!(!pid_path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn stop_running_does_not_signal_unrecognized_pid_file() {
        let _tree_test = PROCESS_TREE_TEST_LOCK.lock().expect("tree test lock");
        let temp = tempfile::tempdir().expect("tempdir");
        let daemon_dir = temp.path().join("daemon");
        std::fs::create_dir_all(&daemon_dir).expect("daemon dir");
        let pid_path = daemon_dir.join("daemon.pid");
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("sleep child");
        std::fs::write(&pid_path, child.id().to_string()).expect("pid file");

        assert!(!stop_running(temp.path()));
        assert!(pid_alive(child.id()));
        assert!(!pid_path.exists());

        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn run_blocking_broadcasts_workflow_run_events_for_ship_state_changes() {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        let state = ShipState::new(151, "owner/repo", "feature/x", "main", "a".repeat(40), "p1");
        store.save(&state).expect("save");

        let state_dir = temp.path().to_path_buf();
        let worker = std::thread::spawn(move || {
            run_blocking(DaemonRunConfig {
                mode: RuntimeMode::Isolated,
                global_dir: state_dir.clone(),
                state_dir,
                repos: vec!["owner/repo".to_owned()],
            })
            .expect("daemon runtime");
            ExitCode::SUCCESS
        });

        let deadline = Instant::now() + Duration::from_secs(2);
        while read_daemon_status(temp.path()).is_none() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }

        let socket_path = temp.path().join("daemon").join("daemon.sock");
        let mut stream = UnixStream::connect(&socket_path).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .expect("timeout");
        stream
            .write_all(br#"{"type":"subscribe"}"#)
            .expect("subscribe");
        stream.write_all(b"\n").expect("newline");

        let mut reader = BufReader::new(stream);
        let mut hello = String::new();
        reader.read_line(&mut hello).expect("hello");
        let hello_value: Value = serde_json::from_str(hello.trim()).expect("hello json");
        assert_eq!(hello_value["type"], "hello");

        let mut updated = store.get(151).expect("state");
        updated.append_run(DispatchedRun {
            target: "macos".to_owned(),
            provider: "github".to_owned(),
            run_id: "42".to_owned(),
            status: "in_progress".to_owned(),
            started_at: Utc::now(),
            updated_at: Utc::now(),
            attempt: 1,
            last_heartbeat_at: None,
            phase: Some("queued".to_owned()),
            required: true,
        });
        store.save(&updated).expect("save");

        let event = read_event(&mut reader).expect("workflow event");
        assert_eq!(event["type"], "event");
        assert_eq!(event["kind"], "workflow_run");
        assert_eq!(event["payload"]["repo"], "owner/repo");
        assert_eq!(event["payload"]["run_id"], 42);
        assert_eq!(event["payload"]["status"], "in_progress");
        assert_eq!(event["payload"]["workflow_name"], "macos");

        let status = read_daemon_status(temp.path()).expect("status");
        assert!(status["last_event_at"].is_number());

        assert!(stop_running(temp.path()));
        assert_eq!(worker.join().expect("join"), ExitCode::SUCCESS);
    }

    #[cfg(unix)]
    fn read_event(reader: &mut BufReader<UnixStream>) -> Option<Value> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut line = String::new();
        while Instant::now() < deadline {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => return None,
                Ok(_) => {
                    let value = serde_json::from_str::<Value>(line.trim()).ok()?;
                    if value.get("type").and_then(Value::as_str) == Some("event") {
                        return Some(value);
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => return None,
            }
        }
        None
    }
}
