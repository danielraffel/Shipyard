use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::*;
use crate::workstream_activation_loader::{ReadyWorkstreamActivation, WorkstreamActivationRefusal};
use crate::workstream_continuation_config::ProviderWrapperConfig;

struct SequenceActivation(Mutex<VecDeque<WorkstreamActivationState>>);

impl ActivationAuthority for SequenceActivation {
    fn revalidate(&mut self) -> WorkstreamActivationState {
        self.0.lock().expect("activation").pop_front().unwrap_or(
            WorkstreamActivationState::Refused(WorkstreamActivationRefusal::ActivationDrift),
        )
    }
}

struct RecordingExecutor {
    selections: Arc<AtomicUsize>,
    executions: Arc<AtomicUsize>,
    uncertain: Option<String>,
    pending: bool,
    unresolved_uncertain: bool,
    block: Option<Arc<AtomicBool>>,
    observed: Arc<Mutex<Vec<ContinuationAction>>>,
    result: ContinuationTickResult,
}

impl ContinuationExecutor for RecordingExecutor {
    fn next_uncertain(
        &self,
        _state_dir: &Path,
        _config: &WorkstreamContinuationConfig,
    ) -> Result<Option<String>, ContinuationTickError> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        Ok(self.uncertain.clone())
    }

    fn has_pending(
        &self,
        _state_dir: &Path,
        _config: &WorkstreamContinuationConfig,
    ) -> Result<bool, ContinuationTickError> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        Ok(self.pending)
    }

    fn has_unresolved_uncertain(
        &self,
        _state_dir: &Path,
        _config: &WorkstreamContinuationConfig,
    ) -> Result<bool, ContinuationTickError> {
        self.selections.fetch_add(1, Ordering::SeqCst);
        Ok(self.unresolved_uncertain)
    }

    fn execute(
        &mut self,
        _state_dir: &Path,
        _config: WorkstreamContinuationConfig,
        action: ContinuationAction,
    ) -> Result<ContinuationTickResult, ContinuationTickError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        self.observed.lock().expect("observed").push(action);
        if let Some(block) = &self.block {
            while block.load(Ordering::Acquire) {
                thread::yield_now();
            }
        }
        Ok(self.result)
    }
}

fn ready() -> WorkstreamActivationState {
    WorkstreamActivationState::Ready(ReadyWorkstreamActivation {
        machine_tag: "m5".to_owned(),
        config: WorkstreamContinuationConfig {
            origin_machine: "m5".to_owned(),
            repositories: vec!["owner/repo".to_owned()],
            provider_wrapper: ProviderWrapperConfig {
                executable_path: PathBuf::from("/opt/wrapper"),
                executable_sha256: "a".repeat(64),
                provider_id: "codex".to_owned(),
                adapter_id: "codex-wrapper-v1".to_owned(),
                deadline_seconds: 30,
                max_stdout_bytes: 1024,
                max_stderr_bytes: 1024,
            },
            terminal_trust: Box::new(crate::workstream_continuation_config::TerminalTrustConfig {
                cmux_signing_team_id: "7WLXT3NR37".to_owned(),
            }),
        },
    })
}

#[allow(clippy::type_complexity)]
fn runtime(
    states: Vec<WorkstreamActivationState>,
    action: Option<ContinuationAction>,
    block: Option<Arc<AtomicBool>>,
) -> (
    WorkstreamContinuationRuntime,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<ContinuationAction>>>,
) {
    let selections = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let (uncertain, pending) = match action {
        Some(ContinuationAction::ReconcileUncertain(wake_id)) => (Some(wake_id), true),
        Some(ContinuationAction::ConsumePending) => (None, true),
        None => (None, false),
    };
    let runtime = WorkstreamContinuationRuntime::new(
        PathBuf::from("/unused"),
        Box::new(SequenceActivation(Mutex::new(states.into()))),
        Box::new(RecordingExecutor {
            selections: Arc::clone(&selections),
            executions: Arc::clone(&executions),
            uncertain,
            pending,
            unresolved_uncertain: false,
            block,
            observed: Arc::clone(&observed),
            result: ContinuationTickResult::Delivered,
        }),
    );
    (runtime, selections, executions, observed)
}

#[allow(clippy::needless_pass_by_value)]
fn wait_for(runtime: &mut WorkstreamContinuationRuntime, expected: ContinuationRuntimeState) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while runtime.status().state != expected && Instant::now() < deadline {
        runtime.tick();
        thread::yield_now();
    }
    assert_eq!(runtime.status().state, expected);
}

#[test]
fn disabled_and_refused_never_select_or_execute() {
    for state in [
        WorkstreamActivationState::Disabled,
        WorkstreamActivationState::Refused(WorkstreamActivationRefusal::InvalidMachinePolicy),
    ] {
        let (mut runtime, selections, executions, _) =
            runtime(vec![state], Some(ContinuationAction::ConsumePending), None);
        runtime.tick();
        assert_eq!(selections.load(Ordering::SeqCst), 0);
        assert_eq!(executions.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn isolated_daemon_never_reads_production_activation() {
    let mut runtime =
        WorkstreamContinuationRuntime::for_daemon(RuntimeMode::Isolated, PathBuf::from("/unused"));
    runtime.tick();
    assert_eq!(runtime.status().state, ContinuationRuntimeState::Disabled);
}

#[test]
fn enabled_empty_controller_reports_ready_not_disabled_or_in_flight() {
    let (mut runtime, selections, executions, _) = runtime(vec![ready()], None, None);
    runtime.tick();
    assert_eq!(runtime.status().state, ContinuationRuntimeState::Ready);
    assert_eq!(runtime.status().reason_code, None);
    assert_eq!(selections.load(Ordering::SeqCst), 3);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    assert_eq!(
        serde_json::to_value(runtime.status()).expect("status JSON")["state"],
        "ready"
    );
}

#[test]
fn exhausted_uncertainty_is_truthful_and_does_not_execute() {
    let selections = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let mut runtime = WorkstreamContinuationRuntime::new(
        PathBuf::from("/unused"),
        Box::new(SequenceActivation(Mutex::new(vec![ready()].into()))),
        Box::new(RecordingExecutor {
            selections: Arc::clone(&selections),
            executions: Arc::clone(&executions),
            uncertain: None,
            pending: false,
            unresolved_uncertain: true,
            block: None,
            observed: Arc::new(Mutex::new(Vec::new())),
            result: ContinuationTickResult::Delivered,
        }),
    );
    runtime.tick();
    assert_eq!(runtime.status().state, ContinuationRuntimeState::Uncertain);
    assert_eq!(
        runtime.status().reason_code.as_deref(),
        Some("reconciliation_budget_exhausted")
    );
    assert_eq!(selections.load(Ordering::SeqCst), 3);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

#[test]
fn activation_drift_latches_before_the_selected_action() {
    let (mut runtime, _, executions, _) = runtime(
        vec![
            ready(),
            WorkstreamActivationState::Refused(WorkstreamActivationRefusal::ActivationDrift),
            WorkstreamActivationState::Refused(WorkstreamActivationRefusal::ActivationDrift),
        ],
        Some(ContinuationAction::ConsumePending),
        None,
    );
    runtime.tick();
    assert_eq!(runtime.status().state, ContinuationRuntimeState::Refused);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    runtime.tick();
    assert_eq!(
        runtime.status().reason_code.as_deref(),
        Some("activation_drift")
    );
}

#[test]
fn dispatch_does_not_depend_on_subscribers() {
    let (mut runtime, _, executions, _) = runtime(
        vec![ready(), ready(), ready()],
        Some(ContinuationAction::ConsumePending),
        None,
    );
    runtime.tick();
    wait_for(&mut runtime, ContinuationRuntimeState::Delivered);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
}

#[test]
fn one_worker_remains_in_flight_until_completion() {
    let block = Arc::new(AtomicBool::new(true));
    let (mut runtime, selections, executions, _) = runtime(
        vec![ready(), ready(), ready()],
        Some(ContinuationAction::ConsumePending),
        Some(Arc::clone(&block)),
    );
    runtime.tick();
    while executions.load(Ordering::SeqCst) == 0 {
        thread::yield_now();
    }
    for _ in 0..10 {
        runtime.tick();
    }
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.status().state, ContinuationRuntimeState::InFlight);
    block.store(false, Ordering::Release);
    wait_for(&mut runtime, ContinuationRuntimeState::Delivered);
}

#[test]
fn uncertain_action_is_preserved_and_executed_first() {
    let action = ContinuationAction::ReconcileUncertain("wake:redacted".to_owned());
    let (mut runtime, _, _, observed) =
        runtime(vec![ready(), ready(), ready()], Some(action.clone()), None);
    runtime.tick();
    wait_for(&mut runtime, ContinuationRuntimeState::Delivered);
    assert_eq!(observed.lock().expect("observed").as_slice(), &[action]);
    let json = serde_json::to_string(&runtime.status()).expect("status JSON");
    assert!(!json.contains("wake:redacted"));
}

#[test]
fn retry_cooldown_prevents_repeated_provider_actions() {
    let selections = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = WorkstreamContinuationRuntime::new_with_action_cooldown(
        PathBuf::from("/unused"),
        Box::new(SequenceActivation(Mutex::new(
            std::iter::repeat_with(ready).take(32).collect(),
        ))),
        Box::new(RecordingExecutor {
            selections: Arc::clone(&selections),
            executions: Arc::clone(&executions),
            uncertain: None,
            pending: true,
            unresolved_uncertain: false,
            block: None,
            observed: Arc::clone(&observed),
            result: ContinuationTickResult::Retrying,
        }),
        Duration::from_secs(30),
    );
    runtime.tick();
    wait_for(&mut runtime, ContinuationRuntimeState::Retrying);
    let selections_after_retry = selections.load(Ordering::SeqCst);
    for _ in 0..20 {
        runtime.tick();
    }
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(selections.load(Ordering::SeqCst), selections_after_retry);
    assert_eq!(observed.lock().expect("observed").len(), 1);
}

#[test]
fn uncertain_cooldown_prevents_repeated_hundred_millisecond_actions() {
    let selections = Arc::new(AtomicUsize::new(0));
    let executions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let mut runtime = WorkstreamContinuationRuntime::new_with_action_cooldown(
        PathBuf::from("/unused"),
        Box::new(SequenceActivation(Mutex::new(
            std::iter::repeat_with(ready).take(32).collect(),
        ))),
        Box::new(RecordingExecutor {
            selections: Arc::clone(&selections),
            executions: Arc::clone(&executions),
            uncertain: Some("wake:redacted".to_owned()),
            pending: false,
            unresolved_uncertain: true,
            block: None,
            observed: Arc::clone(&observed),
            result: ContinuationTickResult::Uncertain,
        }),
        Duration::from_secs(2),
    );
    runtime.tick();
    wait_for(&mut runtime, ContinuationRuntimeState::Uncertain);
    let selections_after_uncertain = selections.load(Ordering::SeqCst);
    for _ in 0..3 {
        thread::sleep(Duration::from_millis(100));
        runtime.tick();
    }
    assert_eq!(runtime.status().state, ContinuationRuntimeState::Uncertain);
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert_eq!(
        selections.load(Ordering::SeqCst),
        selections_after_uncertain
    );
    assert_eq!(observed.lock().expect("observed").len(), 1);
}
