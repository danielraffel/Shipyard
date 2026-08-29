//! Subscriber-independent, single-flight daemon lane for durable continuations.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;

use crate::app::{LaunchProfileV1, decode_protected_launch_profile};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::provider_wrapper::{
    CmuxEndpointV1, FreshResumeExpectationV1, ProviderDeliveryFenceV1, ProviderDeliveryTargetV1,
    ProviderLaunchOptionsV1, ProviderWrapperEnvironment, ProviderWrapperOperationV1,
    ProviderWrapperRequestV1, ProviderWrapperRunResult, TerminalEndpointV1,
    provider_wrapper_execution_supported, run_provider_wrapper,
};
use crate::terminal_delivery_authority::{
    ProductionTerminalEvidenceAdapter, TerminalCapabilityRefusal, TerminalEvidenceAdapter,
};
use crate::work_ledger::{
    DeliveryAuthorityExpectation, DeliveryAuthorityProbe, DeliveryAuthorityRefusal,
    DeliveryAuthorization, DeliveryFence, ExactProtectedProfileResolver, FreshAgentLaunchProfile,
    GitHubAuthorityObservation, ProcessIncarnation, ProviderAdapter,
    ProviderAuthorizationOperation, ProviderCapability, ProviderLaunchRequest, ProviderOutcome,
    ReconciliationAuthorization, StoredProviderRequest, TerminalAuthorityObservation,
    TerminalMutationEndpoint, WakeConsumerPolicy, WakeDeliveryResult, WorkLedger,
    verify_delivery_authority, verify_delivery_or_fresh_authority, verify_reconciliation_authority,
};
use crate::workstream_activation_loader::{WorkstreamActivationLoader, WorkstreamActivationState};
use crate::workstream_continuation_config::WorkstreamContinuationConfig;

/// Redacted lane state exposed through daemon status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationRuntimeState {
    Disabled,
    Refused,
    ProviderUnavailable,
    Ready,
    InFlight,
    Delivered,
    Retrying,
    Uncertain,
    Failed,
    Error,
}

/// Non-sensitive status snapshot. It deliberately contains no route or wake ID.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ContinuationRuntimeStatus {
    pub(crate) state: ContinuationRuntimeState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<String>,
}

impl Default for ContinuationRuntimeStatus {
    fn default() -> Self {
        Self {
            state: ContinuationRuntimeState::Disabled,
            reason_code: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ContinuationAction {
    ReconcileUncertain(String),
    ConsumePending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContinuationTickResult {
    Empty,
    Delivered,
    Retrying,
    Uncertain,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ContinuationTickError(pub(crate) &'static str);

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

pub(crate) trait ContinuationExecutor: Send {
    fn next_uncertain(
        &self,
        state_dir: &Path,
        config: &WorkstreamContinuationConfig,
    ) -> Result<Option<String>, ContinuationTickError>;

    fn has_pending(
        &self,
        state_dir: &Path,
        config: &WorkstreamContinuationConfig,
    ) -> Result<bool, ContinuationTickError>;

    fn has_unresolved_uncertain(
        &self,
        state_dir: &Path,
        config: &WorkstreamContinuationConfig,
    ) -> Result<bool, ContinuationTickError>;

    fn execute(
        &mut self,
        state_dir: &Path,
        config: WorkstreamContinuationConfig,
        action: ContinuationAction,
    ) -> Result<ContinuationTickResult, ContinuationTickError>;

    fn available(&self) -> bool {
        true
    }
}

struct WorkLedgerContinuationExecutor;

impl ContinuationExecutor for WorkLedgerContinuationExecutor {
    fn next_uncertain(
        &self,
        state_dir: &Path,
        config: &WorkstreamContinuationConfig,
    ) -> Result<Option<String>, ContinuationTickError> {
        let Some(ledger) = open_ledger(state_dir)? else {
            return Ok(None);
        };
        ledger
            .next_uncertain_wake_id(&consumer_policy(config))
            .map_err(|_| ContinuationTickError("ledger_select_refused"))
    }

    fn has_pending(
        &self,
        state_dir: &Path,
        config: &WorkstreamContinuationConfig,
    ) -> Result<bool, ContinuationTickError> {
        let Some(ledger) = open_ledger(state_dir)? else {
            return Ok(false);
        };
        ledger
            .has_authorized_pending_wake(&consumer_policy(config))
            .map_err(|_| ContinuationTickError("ledger_status_refused"))
    }

    fn has_unresolved_uncertain(
        &self,
        state_dir: &Path,
        config: &WorkstreamContinuationConfig,
    ) -> Result<bool, ContinuationTickError> {
        let Some(ledger) = open_ledger(state_dir)? else {
            return Ok(false);
        };
        ledger
            .has_authorized_unresolved_uncertain_wake(&consumer_policy(config))
            .map_err(|_| ContinuationTickError("ledger_status_refused"))
    }

    fn execute(
        &mut self,
        state_dir: &Path,
        config: WorkstreamContinuationConfig,
        action: ContinuationAction,
    ) -> Result<ContinuationTickResult, ContinuationTickError> {
        let Some(ledger) = open_ledger(state_dir)? else {
            return Ok(ContinuationTickResult::Empty);
        };
        let environment = provider_environment()?;
        let mut adapter = WorkLedgerProviderAdapter {
            ledger: &ledger,
            config: &config,
            environment,
        };
        let policy = consumer_policy(&config);
        let result = match action {
            ContinuationAction::ReconcileUncertain(wake_id) => {
                ledger.reconcile_uncertain_wake(&policy, &wake_id, &mut adapter)
            }
            ContinuationAction::ConsumePending => {
                let mut resolver =
                    ExactProtectedProfileResolver::new(&ledger, decode_protected_launch_profile);
                ledger.consume_one_wake(policy, &mut resolver, &mut adapter)
            }
        }
        .map_err(|_| ContinuationTickError("ledger_delivery_refused"))?;
        Ok(map_delivery_result(result))
    }

    fn available(&self) -> bool {
        provider_wrapper_execution_supported()
    }
}

fn open_ledger(state_dir: &Path) -> Result<Option<WorkLedger>, ContinuationTickError> {
    WorkLedger::open_existing(state_dir).map_err(|_| ContinuationTickError("ledger_open_refused"))
}

fn consumer_policy(config: &WorkstreamContinuationConfig) -> WakeConsumerPolicy {
    WakeConsumerPolicy {
        activation_enabled: true,
        dispatch_enabled: true,
        authorized_repositories: config.repositories.clone(),
    }
}

fn provider_environment() -> Result<ProviderWrapperEnvironment, ContinuationTickError> {
    let entries = ["HOME", "TMPDIR", "SYSTEMROOT", "USERPROFILE"]
        .into_iter()
        .filter_map(|key| std::env::var_os(key).map(|value| (key.to_owned(), value)));
    ProviderWrapperEnvironment::new(entries)
        .map_err(|_| ContinuationTickError("provider_environment_refused"))
}

fn map_delivery_result(result: WakeDeliveryResult) -> ContinuationTickResult {
    match result {
        WakeDeliveryResult::Empty => ContinuationTickResult::Empty,
        WakeDeliveryResult::Delivered => ContinuationTickResult::Delivered,
        WakeDeliveryResult::Retrying => ContinuationTickResult::Retrying,
        WakeDeliveryResult::Uncertain => ContinuationTickResult::Uncertain,
        WakeDeliveryResult::Failed => ContinuationTickResult::Failed,
    }
}

struct WorkLedgerProviderAdapter<'a> {
    ledger: &'a WorkLedger,
    config: &'a WorkstreamContinuationConfig,
    environment: ProviderWrapperEnvironment,
}

impl ProviderAdapter for WorkLedgerProviderAdapter<'_> {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
        let wrapper = &self.config.provider_wrapper;
        (provider_id == wrapper.provider_id).then(|| ProviderCapability {
            adapter_id: wrapper.adapter_id.clone(),
            fresh_agent_launch: true,
            idempotent_launch: true,
        })
    }

    fn authorize(
        &mut self,
        fence: &DeliveryFence,
        operation: ProviderAuthorizationOperation,
    ) -> Result<DeliveryAuthorization, ProviderOutcome> {
        let wrapper_operation = match operation {
            ProviderAuthorizationOperation::Submit => ProviderWrapperOperationV1::Submit,
            ProviderAuthorizationOperation::Reconcile => ProviderWrapperOperationV1::Reconcile,
        };
        let request = self
            .ledger
            .current_delivery_authority_request(fence)
            .map_err(|error| {
                authority_refusal(
                    wrapper_operation,
                    map_authority_request_error(&error.to_string()),
                )
            })?;
        let cwd = std::env::current_dir().map_err(|_| {
            authority_refusal(
                wrapper_operation,
                DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable,
            )
        })?;
        let trusted_config =
            LoadedConfig::load_machine_global(RuntimeMode::Shipyard).map_err(|_| {
                authority_refusal(
                    wrapper_operation,
                    DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable,
                )
            })?;
        let mut probe = ProductionDeliveryAuthorityProbe {
            github: GitHubActions::from_loaded_config(cwd.clone(), &trusted_config)
                .with_repo_override(&request.expected.repository),
            terminal: Some(request.terminal),
            terminal_adapter: ProductionTerminalEvidenceAdapter,
        };
        let authority = if operation == ProviderAuthorizationOperation::Submit {
            let fresh = self
                .ledger
                .current_reconciliation_authority_request(fence)
                .map_err(|error| {
                    authority_refusal(
                        wrapper_operation,
                        map_authority_request_error(&error.to_string()),
                    )
                })?;
            if fresh.expected != request.expected {
                return Err(authority_refusal(
                    wrapper_operation,
                    DeliveryAuthorityRefusal::GenerationMismatch,
                ));
            }
            verify_delivery_or_fresh_authority(
                &mut probe,
                &request.expected,
                fresh.terminal_endpoint,
                &fresh.fence_digest,
            )
        } else {
            verify_delivery_authority(&mut probe, &request.expected)
        };
        authority.map_err(|refusal| authority_refusal(wrapper_operation, refusal))
    }

    fn authorize_reconciliation(
        &mut self,
        fence: &DeliveryFence,
    ) -> Result<ReconciliationAuthorization, ProviderOutcome> {
        let operation = ProviderWrapperOperationV1::Reconcile;
        let request = self
            .ledger
            .current_reconciliation_authority_request(fence)
            .map_err(|error| {
                authority_refusal(operation, map_authority_request_error(&error.to_string()))
            })?;
        let cwd = std::env::current_dir().map_err(|_| {
            authority_refusal(
                operation,
                DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable,
            )
        })?;
        let trusted_config =
            LoadedConfig::load_machine_global(RuntimeMode::Shipyard).map_err(|_| {
                authority_refusal(
                    operation,
                    DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable,
                )
            })?;
        let mut probe = ProductionDeliveryAuthorityProbe {
            github: GitHubActions::from_loaded_config(cwd, &trusted_config)
                .with_repo_override(&request.expected.repository),
            // Reconciliation must never acquire authority from the dead
            // occupant. If the verifier regresses and asks for it, the probe
            // fails closed rather than consulting requested labels.
            terminal: None,
            terminal_adapter: ProductionTerminalEvidenceAdapter,
        };
        verify_reconciliation_authority(
            &mut probe,
            &request.expected,
            request.terminal_endpoint,
            request.fence_digest,
        )
        .map_err(|refusal| authority_refusal(operation, refusal))
    }

    fn launch(
        &mut self,
        request: ProviderLaunchRequest<'_>,
        authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        self.run(request.fence, ProviderWrapperOperationV1::Submit, authority)
    }

    fn reconcile(
        &mut self,
        _fence: &DeliveryFence,
        _authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        ProviderOutcome::Uncertain {
            evidence: b"legacy reconciliation authority refused".to_vec(),
        }
    }

    fn reconcile_read_only(
        &mut self,
        fence: &DeliveryFence,
        authority: ReconciliationAuthorization,
    ) -> ProviderOutcome {
        let Ok(current) = self.ledger.current_reconciliation_authority_request(fence) else {
            return preflight_refusal(ProviderWrapperOperationV1::Reconcile);
        };
        if current.fence_digest != authority.fence_digest()
            || &current.terminal_endpoint != authority.terminal_endpoint()
            || authority.receipt_digest().len() != 64
        {
            return preflight_refusal(ProviderWrapperOperationV1::Reconcile);
        }
        self.run_reconciliation(fence, &authority)
    }
}

struct ProductionDeliveryAuthorityProbe {
    github: GitHubActions,
    terminal: Option<crate::terminal_delivery_authority::TerminalCapabilityRequest>,
    terminal_adapter: ProductionTerminalEvidenceAdapter,
}

impl DeliveryAuthorityProbe for ProductionDeliveryAuthorityProbe {
    fn observe_github(
        &mut self,
        expected: &DeliveryAuthorityExpectation,
    ) -> Result<GitHubAuthorityObservation, DeliveryAuthorityRefusal> {
        let installation_id = self
            .github
            .app_installation_id()
            .map_err(|_| DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable)?;
        let raw = self
            .github
            .run_gh_with_timeout(
                &[
                    "pr".into(),
                    "view".into(),
                    expected.pull_request.to_string(),
                    "--repo".into(),
                    expected.repository.clone(),
                    "--json".into(),
                    "state,headRefOid,baseRefName,baseRefOid".into(),
                ],
                Duration::from_secs(15),
            )
            .map_err(|_| DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable)?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|_| DeliveryAuthorityRefusal::GitHubAppAuthorityUnavailable)?;
        if value.get("state").and_then(Value::as_str) != Some("OPEN") {
            return Err(DeliveryAuthorityRefusal::HeadMismatch);
        }
        Ok(GitHubAuthorityObservation {
            app_authenticated: true,
            installation_id,
            repository: expected.repository.clone(),
            pull_request: expected.pull_request,
            head_sha: value
                .get("headRefOid")
                .and_then(Value::as_str)
                .ok_or(DeliveryAuthorityRefusal::HeadMismatch)?
                .to_ascii_lowercase(),
            base_ref: value
                .get("baseRefName")
                .and_then(Value::as_str)
                .ok_or(DeliveryAuthorityRefusal::BaseRefMissing)?
                .to_owned(),
            base_sha: value
                .get("baseRefOid")
                .and_then(Value::as_str)
                .ok_or(DeliveryAuthorityRefusal::BaseRefMissing)?
                .to_ascii_lowercase(),
            observed_at: Utc::now(),
        })
    }

    fn verify_terminal_once(
        &mut self,
        expected: &DeliveryAuthorityExpectation,
    ) -> Result<TerminalAuthorityObservation, DeliveryAuthorityRefusal> {
        let terminal_request = self
            .terminal
            .as_ref()
            .ok_or(DeliveryAuthorityRefusal::TerminalAuthorityUnavailable)?;
        let observed = self
            .terminal_adapter
            .verify_once(terminal_request)
            .map_err(map_terminal_refusal)?;
        let mutation_endpoint = match terminal_request {
            crate::terminal_delivery_authority::TerminalCapabilityRequest::Cmux {
                cli_path,
                socket_path,
                ..
            } => TerminalMutationEndpoint::Cmux {
                executable_path: cli_path.clone(),
                socket_path: socket_path.clone(),
            },
            crate::terminal_delivery_authority::TerminalCapabilityRequest::HerdR { .. } => {
                return Err(DeliveryAuthorityRefusal::TerminalAuthorityUnavailable);
            }
        };
        Ok(TerminalAuthorityObservation {
            requested_terminal_instance: expected.requested_terminal_instance.clone(),
            actual_terminal_instance: observed.terminal_instance,
            process: ProcessIncarnation {
                boot_id: observed.process.boot_id,
                pid: observed.process.pid,
                start_identity: observed.process.start_identity,
            },
            native_session_id: observed.native_session_id,
            mutation_endpoint,
            observed_at: Utc::now(),
        })
    }
}

fn map_terminal_refusal(value: TerminalCapabilityRefusal) -> DeliveryAuthorityRefusal {
    match value {
        TerminalCapabilityRefusal::MethodMissing => DeliveryAuthorityRefusal::MethodMissing,
        TerminalCapabilityRefusal::NoMatch => DeliveryAuthorityRefusal::NoTerminalMatch,
        TerminalCapabilityRefusal::MultipleMatches => {
            DeliveryAuthorityRefusal::MultipleTerminalMatches
        }
        TerminalCapabilityRefusal::ProcessIncarnationChanged => {
            DeliveryAuthorityRefusal::ProcessIncarnationMismatch
        }
        TerminalCapabilityRefusal::NativeSessionMismatch => {
            DeliveryAuthorityRefusal::NativeSessionMismatch
        }
        TerminalCapabilityRefusal::Unsupported
        | TerminalCapabilityRefusal::Unobservable
        | TerminalCapabilityRefusal::InvalidResponse => {
            DeliveryAuthorityRefusal::TerminalAuthorityUnavailable
        }
    }
}

fn map_authority_request_error(error: &str) -> DeliveryAuthorityRefusal {
    for refusal in [
        DeliveryAuthorityRefusal::DirectProviderForbidden,
        DeliveryAuthorityRefusal::StaticRouteMetadataOnly,
        DeliveryAuthorityRefusal::BaseRefMissing,
    ] {
        if error.contains(refusal.code()) {
            return refusal;
        }
    }
    DeliveryAuthorityRefusal::TerminalAuthorityUnavailable
}

impl WorkLedgerProviderAdapter<'_> {
    fn run(
        &self,
        fence: &DeliveryFence,
        operation: ProviderWrapperOperationV1,
        authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        let delivery_target = if authority.is_fresh_checkpoint() {
            ProviderDeliveryTargetV1::FreshCheckpoint
        } else {
            ProviderDeliveryTargetV1::OriginalSession {
                surface_id: authority.terminal_instance().to_owned(),
            }
        };
        let Ok(mutation_endpoint) =
            authority.into_mutation_endpoint_for(fence.work_generation, fence.owner_generation)
        else {
            return preflight_refusal(operation);
        };
        let Ok(request) =
            self.wrapper_request(fence, operation, &mutation_endpoint, delivery_target)
        else {
            return preflight_refusal(operation);
        };
        match run_provider_wrapper(&self.config.provider_wrapper, &self.environment, &request) {
            Ok(ProviderWrapperRunResult::Delivered {
                response_receipt, ..
            }) => ProviderOutcome::Delivered {
                receipt: response_receipt.canonical_bytes,
            },
            Ok(ProviderWrapperRunResult::Retryable {
                response_receipt, ..
            }) => ProviderOutcome::Retryable {
                evidence: response_receipt.canonical_bytes,
            },
            Ok(ProviderWrapperRunResult::Rejected {
                response_receipt, ..
            }) => match operation {
                ProviderWrapperOperationV1::Submit => ProviderOutcome::Rejected {
                    evidence: response_receipt.canonical_bytes,
                },
                ProviderWrapperOperationV1::Reconcile => ProviderOutcome::NotDelivered {
                    evidence: response_receipt.canonical_bytes,
                },
            },
            Ok(ProviderWrapperRunResult::Uncertain {
                response_receipt, ..
            }) => ProviderOutcome::Uncertain {
                evidence: response_receipt.map_or_else(
                    || b"provider-wrapper-uncertain-without-receipt".to_vec(),
                    |receipt| receipt.canonical_bytes,
                ),
            },
            Err(_) => preflight_refusal(operation),
        }
    }

    #[allow(clippy::needless_pass_by_value)] // Consumes the one-shot capability at provider I/O.
    fn run_reconciliation(
        &self,
        fence: &DeliveryFence,
        authority: &ReconciliationAuthorization,
    ) -> ProviderOutcome {
        let operation = ProviderWrapperOperationV1::Reconcile;
        let Ok(request) = self.wrapper_request(
            fence,
            operation,
            authority.terminal_endpoint(),
            ProviderDeliveryTargetV1::ReconcileOnly,
        ) else {
            return preflight_refusal(operation);
        };
        match run_provider_wrapper(&self.config.provider_wrapper, &self.environment, &request) {
            Ok(ProviderWrapperRunResult::Delivered {
                response_receipt, ..
            }) => ProviderOutcome::Delivered {
                receipt: response_receipt.canonical_bytes,
            },
            Ok(ProviderWrapperRunResult::Rejected {
                response_receipt, ..
            }) => ProviderOutcome::NotDelivered {
                evidence: response_receipt.canonical_bytes,
            },
            Ok(ProviderWrapperRunResult::Retryable {
                response_receipt, ..
            }) => ProviderOutcome::Uncertain {
                evidence: response_receipt.canonical_bytes,
            },
            Ok(ProviderWrapperRunResult::Uncertain {
                response_receipt, ..
            }) => ProviderOutcome::Uncertain {
                evidence: response_receipt.map_or_else(
                    || b"provider-wrapper-uncertain-without-receipt".to_vec(),
                    |receipt| receipt.canonical_bytes,
                ),
            },
            Err(_) => preflight_refusal(operation),
        }
    }

    fn wrapper_request(
        &self,
        fence: &DeliveryFence,
        operation: ProviderWrapperOperationV1,
        mutation_endpoint: &TerminalMutationEndpoint,
        delivery_target: ProviderDeliveryTargetV1,
    ) -> Result<ProviderWrapperRequestV1, ()> {
        let (record, bytes) = self
            .ledger
            .open_protected_object(&fence.request_object_ref)
            .map_err(|_| ())?;
        if record.work_item_id != fence.work_item_id || record.kind != "provider_request" {
            return Err(());
        }
        let stored: StoredProviderRequest = serde_json::from_slice(&bytes).map_err(|_| ())?;
        if stored.schema_version != 2
            || stored.wake_id != fence.wake_id
            || stored.attempt != fence.attempt
            || stored.adapter_id != fence.adapter_id
            || stored.provider_id != fence.provider_id
            || stored.idempotency_key != fence.idempotency_key
            || stored.profile_ref != fence.profile_ref
            || stored.profile_digest != fence.payload_digest
        {
            return Err(());
        }
        let mut resolver =
            ExactProtectedProfileResolver::new(self.ledger, decode_protected_launch_profile);
        let profile: LaunchProfileV1 = resolver
            .resolve_exact(&fence.work_item_id, &fence.payload_digest)
            .map_err(|_| ())?;
        profile
            .validate_native_fresh_agent_grammar()
            .map_err(|_| ())?;
        if profile.provider_id() != fence.provider_id
            || profile.provider_launch_options() != stored.launch_options
        {
            return Err(());
        }
        let mut delivery_fence = ProviderDeliveryFenceV1 {
            wake_id: fence.wake_id.clone(),
            work_item_id: fence.work_item_id.clone(),
            work_generation: fence.work_generation,
            owner_generation: fence.owner_generation,
            route_ref: fence.route_ref.clone(),
            payload_digest: fence.payload_digest.clone(),
            attempt: fence.attempt,
            consumer_epoch: fence.consumer_epoch,
            consumer_owner_ref: fence.consumer_owner_ref.clone(),
            idempotency_key: String::new(),
        };
        delivery_fence.bind_idempotency_key();
        let terminal_endpoint = match mutation_endpoint {
            TerminalMutationEndpoint::Cmux {
                executable_path,
                socket_path,
            } => TerminalEndpointV1::Cmux(CmuxEndpointV1 {
                executable_path: executable_path.clone(),
                socket_path: socket_path.clone(),
                signing_team_id: self.config.terminal_trust.cmux_signing_team_id.clone(),
            }),
        };
        Ok(ProviderWrapperRequestV1 {
            schema_version: crate::provider_wrapper::PROVIDER_WRAPPER_SCHEMA_VERSION,
            operation,
            provider_id: stored.provider_id,
            adapter_id: stored.adapter_id,
            delivery_fence,
            terminal_endpoint,
            delivery_target,
            protected_route: profile.protected_resume_route(fence.payload_digest.clone()),
            resume_expectation: FreshResumeExpectationV1 {
                workstream_handle: stored.resume.workstream_handle,
                context_url: stored.resume.context_url,
                plan_sha256: stored.resume.plan_sha256,
                root_revision: stored.resume.root_revision,
                issue_revision: stored.resume.issue_revision,
                material_event_revision: stored.resume.material_event_revision,
                projection_revision: stored.resume.projection_revision,
                checkpoint_id: stored.resume.checkpoint_id,
                checkpoint_generation: stored.resume.checkpoint_generation,
                checkpoint_digest: stored.resume.checkpoint_digest,
                repository: stored.resume.repository,
                worktree_path: profile.worktree_path().to_owned(),
                head_sha: stored.resume.head_sha,
                expected_resume_context_digest: stored.resume.expected_resume_context_digest,
                success_continuation_digest: stored.resume.success_continuation_digest,
                failure_continuation_digest: stored.resume.failure_continuation_digest,
            },
            launch_options: ProviderLaunchOptionsV1 {
                model_id: stored.launch_options.model_id,
                reasoning_effort: stored.launch_options.reasoning_effort,
            },
        })
    }
}

fn preflight_refusal(operation: ProviderWrapperOperationV1) -> ProviderOutcome {
    match operation {
        ProviderWrapperOperationV1::Submit => ProviderOutcome::Rejected {
            evidence: b"provider-wrapper-preflight-refused".to_vec(),
        },
        ProviderWrapperOperationV1::Reconcile => ProviderOutcome::Uncertain {
            evidence: b"provider-wrapper-reconcile-refused".to_vec(),
        },
    }
}

fn authority_refusal(
    operation: ProviderWrapperOperationV1,
    refusal: DeliveryAuthorityRefusal,
) -> ProviderOutcome {
    let evidence = format!("delivery-authority-refused:{}", refusal.code()).into_bytes();
    match operation {
        ProviderWrapperOperationV1::Submit => ProviderOutcome::Rejected { evidence },
        ProviderWrapperOperationV1::Reconcile => ProviderOutcome::Uncertain { evidence },
    }
}

struct WorkerResult {
    executor: Box<dyn ContinuationExecutor>,
    result: Result<ContinuationTickResult, ContinuationTickError>,
}

/// One nonblocking daemon lane. At most one worker owns its executor.
pub(crate) struct WorkstreamContinuationRuntime {
    state_dir: PathBuf,
    activation: Box<dyn ActivationAuthority>,
    executor: Option<Box<dyn ContinuationExecutor>>,
    worker: Option<Receiver<WorkerResult>>,
    status: ContinuationRuntimeStatus,
    action_not_before: Option<Instant>,
    action_cooldown: Duration,
}

impl WorkstreamContinuationRuntime {
    pub(crate) fn for_daemon(mode: RuntimeMode, state_dir: PathBuf) -> Self {
        Self::for_daemon_with_executor(mode, state_dir, Box::new(WorkLedgerContinuationExecutor))
    }

    pub(crate) fn for_daemon_with_executor(
        mode: RuntimeMode,
        state_dir: PathBuf,
        executor: Box<dyn ContinuationExecutor>,
    ) -> Self {
        let activation: Box<dyn ActivationAuthority> = match mode {
            RuntimeMode::Shipyard => Box::new(WorkstreamActivationLoader::production()),
            RuntimeMode::Isolated => Box::new(DisabledActivation),
        };
        Self::new(state_dir, activation, executor)
    }

    fn new(
        state_dir: PathBuf,
        activation: Box<dyn ActivationAuthority>,
        executor: Box<dyn ContinuationExecutor>,
    ) -> Self {
        Self::new_with_action_cooldown(state_dir, activation, executor, Duration::from_secs(30))
    }

    fn new_with_action_cooldown(
        state_dir: PathBuf,
        activation: Box<dyn ActivationAuthority>,
        executor: Box<dyn ContinuationExecutor>,
        action_cooldown: Duration,
    ) -> Self {
        Self {
            state_dir,
            activation,
            executor: Some(executor),
            worker: None,
            status: ContinuationRuntimeStatus::default(),
            action_not_before: None,
            action_cooldown,
        }
    }

    pub(crate) fn status(&self) -> ContinuationRuntimeStatus {
        self.status.clone()
    }

    /// Poll completion and, when idle, start at most one bounded action.
    pub(crate) fn tick(&mut self) {
        if self.poll_worker() {
            return;
        }
        if self.worker.is_some() {
            self.set_status(ContinuationRuntimeState::InFlight, None);
            return;
        }
        let Some(config) = self.ready_policy() else {
            return;
        };
        let Some(executor) = self.executor.as_ref() else {
            self.set_status(ContinuationRuntimeState::Error, Some("executor_lost"));
            return;
        };
        if !executor.available() {
            self.set_status(
                ContinuationRuntimeState::ProviderUnavailable,
                Some("provider_unavailable"),
            );
            return;
        }
        if self
            .action_not_before
            .is_some_and(|deadline| Instant::now() < deadline)
        {
            if self.status.state != ContinuationRuntimeState::Uncertain {
                self.set_status(ContinuationRuntimeState::Retrying, None);
            }
            return;
        }
        self.action_not_before = None;
        let action = match executor.next_uncertain(&self.state_dir, &config) {
            Ok(Some(wake_id)) => ContinuationAction::ReconcileUncertain(wake_id),
            Ok(None) => match executor.has_pending(&self.state_dir, &config) {
                Ok(true) => ContinuationAction::ConsumePending,
                Ok(false) => match executor.has_unresolved_uncertain(&self.state_dir, &config) {
                    Ok(true) => {
                        self.set_status(
                            ContinuationRuntimeState::Uncertain,
                            Some("reconciliation_budget_exhausted"),
                        );
                        return;
                    }
                    Ok(false) => {
                        self.set_status(ContinuationRuntimeState::Ready, None);
                        return;
                    }
                    Err(error) => {
                        self.set_status(ContinuationRuntimeState::Error, Some(error.0));
                        return;
                    }
                },
                Err(error) => {
                    self.set_status(ContinuationRuntimeState::Error, Some(error.0));
                    return;
                }
            },
            Err(error) => {
                self.set_status(ContinuationRuntimeState::Error, Some(error.0));
                return;
            }
        };

        // Revalidate after selection and immediately before the worker becomes
        // capable of mutation or provider I/O.
        let Some(config) = self.ready_policy() else {
            return;
        };
        let Some(mut executor) = self.executor.take() else {
            self.set_status(ContinuationRuntimeState::Error, Some("executor_lost"));
            return;
        };
        let state_dir = self.state_dir.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let result = executor.execute(&state_dir, config, action);
            let _ = sender.send(WorkerResult { executor, result });
        });
        self.worker = Some(receiver);
        self.set_status(ContinuationRuntimeState::InFlight, None);
    }

    fn ready_policy(&mut self) -> Option<WorkstreamContinuationConfig> {
        match self.activation.revalidate() {
            WorkstreamActivationState::Disabled => {
                self.set_status(ContinuationRuntimeState::Disabled, None);
                None
            }
            WorkstreamActivationState::Refused(reason) => {
                self.set_status(ContinuationRuntimeState::Refused, Some(reason.code()));
                None
            }
            WorkstreamActivationState::Ready(ready) => Some(ready.config),
        }
    }

    fn poll_worker(&mut self) -> bool {
        let Some(receiver) = self.worker.as_ref() else {
            return false;
        };
        let message = match receiver.try_recv() {
            Ok(message) => Some(message),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                self.worker = None;
                self.set_status(ContinuationRuntimeState::Error, Some("worker_lost"));
                return true;
            }
        };
        let Some(message) = message else {
            return false;
        };
        self.worker = None;
        self.executor = Some(message.executor);
        match message.result {
            Ok(ContinuationTickResult::Empty) => {
                self.set_status(ContinuationRuntimeState::Ready, None);
            }
            Ok(ContinuationTickResult::Delivered) => {
                self.set_status(ContinuationRuntimeState::Delivered, None);
            }
            Ok(ContinuationTickResult::Retrying) => {
                self.action_not_before = Some(Instant::now() + self.action_cooldown);
                self.set_status(ContinuationRuntimeState::Retrying, None);
            }
            Ok(ContinuationTickResult::Uncertain) => {
                self.action_not_before = Some(Instant::now() + self.action_cooldown);
                self.set_status(ContinuationRuntimeState::Uncertain, None);
            }
            Ok(ContinuationTickResult::Failed) => {
                self.set_status(ContinuationRuntimeState::Failed, None);
            }
            Err(error) => self.set_status(ContinuationRuntimeState::Error, Some(error.0)),
        }
        true
    }

    fn set_status(&mut self, state: ContinuationRuntimeState, reason: Option<&str>) {
        self.status = ContinuationRuntimeStatus {
            state,
            reason_code: reason.map(ToOwned::to_owned),
        };
    }
}

// Windows does not start this daemon lane yet, but keep the complete runtime
// compile-checked there instead of hiding platform drift behind dead-code
// allowances. This uncalled function roots construction and a complete tick
// for strict cross-target linting without executing either.
#[cfg(not(unix))]
fn compile_check_unsupported_runtime(state_dir: PathBuf) {
    let mut runtime = WorkstreamContinuationRuntime::for_daemon(RuntimeMode::Shipyard, state_dir);
    runtime.tick();
    drop(runtime.status());
}

#[cfg(not(unix))]
const _: fn(PathBuf) = compile_check_unsupported_runtime;

#[cfg(test)]
mod tests;
