//! Subscriber-independent, no-model single-flight continuation lane.
//!
//! The daemon owns this scheduler. Activation is revalidated immediately
//! before each launch, only one worker may exist, and refusal is sticky. The
//! executor boundary contains routine deterministic work only; it has no model
//! or terminal UI dependency.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

use crate::identity::RuntimeMode;
use crate::provider_wrapper::ProviderWrapperEnvironment;
use crate::work_ledger::WorkLedger;
use crate::workstream_activation_loader::{
    ReadyWorkstreamActivation, WorkstreamActivationLoader, WorkstreamActivationState,
};

trait ActivationAuthority: Send {
    fn revalidate(&mut self) -> WorkstreamActivationState;
}

impl ActivationAuthority for WorkstreamActivationLoader {
    fn revalidate(&mut self) -> WorkstreamActivationState {
        self.revalidate_for_tick()
    }
}

struct DisabledActivation;

impl ActivationAuthority for DisabledActivation {
    fn revalidate(&mut self) -> WorkstreamActivationState {
        WorkstreamActivationState::Disabled
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ContinuationRuntimeState {
    Disabled,
    Idle,
    Running,
    Refused(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContinuationTickResult {
    NoWork,
    #[allow(dead_code)] // Produced once the private publication ingress is activated.
    Consumed,
}

pub(crate) trait ContinuationExecutor: Send {
    fn consume_one(
        &mut self,
        activation: &ReadyWorkstreamActivation,
    ) -> Result<ContinuationTickResult, &'static str>;
}

type WorkerResult = (
    Box<dyn ContinuationExecutor>,
    Result<ContinuationTickResult, &'static str>,
);

const CONTINUATION_POLL_INTERVAL: Duration = Duration::from_secs(30);

struct DefaultContinuationExecutor {
    state_dir: PathBuf,
}

impl ContinuationExecutor for DefaultContinuationExecutor {
    fn consume_one(
        &mut self,
        activation: &ReadyWorkstreamActivation,
    ) -> Result<ContinuationTickResult, &'static str> {
        let Some(ledger) =
            WorkLedger::open_existing(&self.state_dir).map_err(|_| "ledger_open_refused")?
        else {
            return Ok(ContinuationTickResult::NoWork);
        };
        let mut environment_entries = ["HOME", "TMPDIR", "SYSTEMROOT", "USERPROFILE"]
            .into_iter()
            .filter_map(|key| std::env::var_os(key).map(|value| (key.to_owned(), value)))
            .collect::<Vec<_>>();
        environment_entries.push((
            "SHIPYARD_PROVIDER_COMPANION_PATH".to_owned(),
            activation
                .config
                .provider_wrapper
                .executable_path
                .as_os_str()
                .to_owned(),
        ));
        let environment = ProviderWrapperEnvironment::new(environment_entries)
            .map_err(|_| "provider_environment_refused")?;
        match ledger
            .native_provider_tick(
                &activation.config.provider_wrapper,
                &environment,
                &activation.config.repositories,
            )
            .map_err(|_| "native_provider_delivery_refused")?
        {
            crate::work_ledger::NativeProviderTickResult::NoWork => {
                Ok(ContinuationTickResult::NoWork)
            }
            crate::work_ledger::NativeProviderTickResult::Progressed => {
                Ok(ContinuationTickResult::Consumed)
            }
        }
    }
}

pub(crate) struct WorkstreamContinuationRuntime {
    activation: Box<dyn ActivationAuthority>,
    executor: Option<Box<dyn ContinuationExecutor>>,
    worker: Option<Receiver<WorkerResult>>,
    state: ContinuationRuntimeState,
    next_activation_at: Instant,
}

impl WorkstreamContinuationRuntime {
    pub(crate) fn new(mode: RuntimeMode, state_dir: PathBuf) -> Self {
        if mode != RuntimeMode::Shipyard {
            return Self {
                activation: Box::new(DisabledActivation),
                executor: Some(Box::new(DefaultContinuationExecutor { state_dir })),
                worker: None,
                state: ContinuationRuntimeState::Disabled,
                next_activation_at: Instant::now(),
            };
        }
        Self {
            activation: Box::new(WorkstreamActivationLoader::production()),
            executor: Some(Box::new(DefaultContinuationExecutor { state_dir })),
            worker: None,
            state: ContinuationRuntimeState::Disabled,
            next_activation_at: Instant::now(),
        }
    }

    #[cfg(test)]
    fn with_parts(
        activation: Box<dyn ActivationAuthority>,
        executor: Box<dyn ContinuationExecutor>,
    ) -> Self {
        Self {
            activation,
            executor: Some(executor),
            worker: None,
            state: ContinuationRuntimeState::Disabled,
            next_activation_at: Instant::now(),
        }
    }

    pub(crate) fn tick(&mut self) {
        if let Some(receiver) = &self.worker {
            match receiver.try_recv() {
                Ok((executor, Ok(_))) => {
                    self.executor = Some(executor);
                    self.worker = None;
                    self.state = ContinuationRuntimeState::Idle;
                    self.next_activation_at = Instant::now() + CONTINUATION_POLL_INTERVAL;
                    return;
                }
                Ok((executor, Err(reason))) => {
                    self.executor = Some(executor);
                    self.worker = None;
                    self.state = ContinuationRuntimeState::Refused(reason);
                }
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.worker = None;
                    self.state = ContinuationRuntimeState::Refused("worker_disconnected");
                }
            }
        }
        if matches!(self.state, ContinuationRuntimeState::Refused(_)) {
            return;
        }
        if Instant::now() < self.next_activation_at {
            return;
        }
        let ready = match self.activation.revalidate() {
            WorkstreamActivationState::Disabled => {
                self.state = ContinuationRuntimeState::Disabled;
                self.next_activation_at = Instant::now() + CONTINUATION_POLL_INTERVAL;
                return;
            }
            WorkstreamActivationState::Refused(reason) => {
                self.state = ContinuationRuntimeState::Refused(reason.code());
                return;
            }
            WorkstreamActivationState::Ready(ready) => ready,
        };
        let Some(mut executor) = self.executor.take() else {
            self.state = ContinuationRuntimeState::Running;
            return;
        };
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = executor.consume_one(&ready);
            let _ = sender.send((executor, result));
        });
        self.worker = Some(receiver);
        self.state = ContinuationRuntimeState::Running;
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &ContinuationRuntimeState {
        &self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workstream_continuation_config::{
        ProviderWrapperConfig, WorkstreamContinuationConfig,
    };
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

    struct ReadyActivation(ReadyWorkstreamActivation);
    impl ActivationAuthority for ReadyActivation {
        fn revalidate(&mut self) -> WorkstreamActivationState {
            WorkstreamActivationState::Ready(self.0.clone())
        }
    }

    struct BlockingExecutor {
        calls: Arc<AtomicUsize>,
        barrier: Arc<Barrier>,
    }
    impl ContinuationExecutor for BlockingExecutor {
        fn consume_one(
            &mut self,
            _activation: &ReadyWorkstreamActivation,
        ) -> Result<ContinuationTickResult, &'static str> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait();
            Ok(ContinuationTickResult::Consumed)
        }
    }

    fn ready() -> ReadyWorkstreamActivation {
        ReadyWorkstreamActivation {
            machine_tag: "m3".to_owned(),
            config: WorkstreamContinuationConfig {
                origin_machine: "m3".to_owned(),
                repositories: vec!["danielraffel/shipyard".to_owned()],
                provider_wrapper: ProviderWrapperConfig {
                    executable_path: "/usr/bin/false".into(),
                    executable_sha256: "a".repeat(64),
                    provider_id: "codex".to_owned(),
                    adapter_id: "subrouter".to_owned(),
                    deadline_seconds: 30,
                    max_stdout_bytes: 4096,
                    max_stderr_bytes: 4096,
                },
            },
        }
    }

    #[test]
    fn isolated_runtime_is_permanently_default_off() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut runtime =
            WorkstreamContinuationRuntime::new(RuntimeMode::Isolated, temp.path().to_path_buf());
        runtime.tick();
        assert_eq!(runtime.state(), &ContinuationRuntimeState::Disabled);
        assert!(runtime.worker.is_none());
    }

    #[test]
    fn daemon_lane_is_single_flight() {
        let calls = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(2));
        let mut runtime = WorkstreamContinuationRuntime::with_parts(
            Box::new(ReadyActivation(ready())),
            Box::new(BlockingExecutor {
                calls: Arc::clone(&calls),
                barrier: Arc::clone(&barrier),
            }),
        );
        runtime.tick();
        let deadline = Instant::now() + Duration::from_secs(2);
        while calls.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::yield_now();
        }
        runtime.tick();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.state(), &ContinuationRuntimeState::Running);
        barrier.wait();
    }
}
