#![allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]

use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::terminal_transport::CommandResult;
use super::*;
use crate::provider_wrapper::{
    CmuxEndpointV1, FreshResumeExpectationV1, ProtectedProviderRouteV1, ProviderDeliveryFenceV1,
    ProviderLaunchOptionsV1, ProviderReasoningEffortV1, TerminalEndpointV1,
};
use crate::work_ledger::{
    DeliveryAuthorization, DeliveryFence, FreshAgentLaunchProfile, FreshAgentProviderLaunchOptions,
    FreshAgentResumeExpectation, ProviderAdapter, ProviderAuthorizationOperation,
    ProviderCapability, ProviderLaunchRequest, ProviderOutcome, ReconciliationAuthorization,
    WakeEnvelope, WakeProfileResolver,
};
#[cfg(unix)]
use crate::work_ledger::{
    NativePublicationRequest, WakeConsumerPolicy, WakeDeliveryResult, WorkLedger,
};

const UUID: &str = "123E4567-E89B-12D3-A456-426614174000";
const OTHER_WINDOW_UUID: &str = "923E4567-E89B-12D3-A456-426614174000";
const SURFACE_UUID: &str = "223E4567-E89B-12D3-A456-426614174000";
const SESSION_UUID: &str = "323E4567-E89B-12D3-A456-426614174000";

fn native_absolute_test_path(leaf: &str) -> String {
    if cfg!(windows) {
        format!(r"C:\Shipyard\{leaf}")
    } else {
        format!("/tmp/shipyard/{leaf}")
    }
}

#[derive(Default)]
struct FakeRunner {
    verification: Option<Result<(), RunnerFailure>>,
    bound_endpoints: Vec<TerminalEndpointV1>,
    results: VecDeque<Result<CommandResult, RunnerFailure>>,
    calls: Vec<Vec<String>>,
    private_launches: Vec<String>,
}

fn private_launch_path(command: &str) -> Option<PathBuf> {
    Some(PathBuf::from(
        command.strip_prefix("'/bin/sh' '")?.strip_suffix('\'')?,
    ))
}

impl TerminalTransport for FakeRunner {
    fn bind(&mut self, endpoint: &TerminalEndpointV1) -> Result<(), RunnerFailure> {
        self.bound_endpoints.push(endpoint.clone());
        self.verification.take().unwrap_or(Ok(()))
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure> {
        self.calls.push(args.to_vec());
        let result = self
            .results
            .pop_front()
            .expect("test runner must provide one result per call");
        let initial_command = (args.get(3).map(String::as_str) == Some("rpc")
            && args.get(4).map(String::as_str) == Some("workspace.create"))
        .then(|| {
            serde_json::from_str::<serde_json::Value>(&args[5]).unwrap()["initial_command"]
                .as_str()
                .unwrap()
                .to_owned()
        });
        if let Some(command) = initial_command
            && let Some(path) = private_launch_path(&command)
        {
            self.private_launches
                .push(std::fs::read_to_string(&path).unwrap());
            if result.as_ref().is_ok_and(|result| result.success) {
                std::fs::remove_file(&path).unwrap();
                std::fs::remove_dir(path.parent().unwrap()).unwrap();
            }
        }
        result
    }
}

#[derive(Default)]
struct FakeProviderLaunchAuthority {
    route_verification: Option<Result<(), &'static str>>,
    verify_calls: usize,
    prepare_calls: usize,
    verified_routes: Vec<(String, String)>,
}

impl ProviderLaunchAuthority for FakeProviderLaunchAuthority {
    fn verify_route(&mut self, request: &ProviderWrapperRequestV1) -> Result<(), &'static str> {
        self.verify_calls += 1;
        self.verified_routes.push((
            request.protected_route.argv[0].clone(),
            request.provider_id.clone(),
        ));
        self.route_verification.take().unwrap_or(Ok(()))
    }

    fn prepare_launch(
        &mut self,
        request: &ProviderWrapperRequestV1,
    ) -> Result<PrivateLaunch, &'static str> {
        self.prepare_calls += 1;
        prepare_private_launch(request, false)
    }
}

fn handle_with_default_provider(
    request: &ProviderWrapperRequestV1,
    terminal: &mut FakeRunner,
) -> ProviderWrapperResponseV1 {
    handle_request(
        request,
        terminal,
        &mut FakeProviderLaunchAuthority::default(),
    )
}

fn successful_json(value: serde_json::Value) -> Result<CommandResult, RunnerFailure> {
    Ok(CommandResult {
        success: true,
        stdout: serde_json::to_vec(&value).unwrap(),
    })
}

fn list(workspaces: serde_json::Value) -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::json!({"window_id": UUID, "workspaces": workspaces}))
}

fn list_for_window(
    window_id: &str,
    workspaces: serde_json::Value,
) -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::json!({"window_id": window_id, "workspaces": workspaces}))
}

fn windows(ids: &[&str]) -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::Value::Array(
        ids.iter()
            .map(|id| serde_json::json!({"id": id, "index": 0, "key": true}))
            .collect(),
    ))
}

fn created() -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::json!({
        "window_id": UUID,
        "workspace_id": UUID,
        "surface_id": SURFACE_UUID,
        "group_id": null
    }))
}

fn workspace_create_capabilities() -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::json!({
        "protocol": "cmux-socket",
        "version": 2,
        "methods": ["workspace.create"],
        "capabilities": ["workspace.task_create.v1"]
    }))
}

#[test]
fn live_create_shape_accepts_additive_metadata_and_requires_uuid_ids() {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "window_id": OTHER_WINDOW_UUID,
        "workspace_id": UUID,
        "surface_id": SURFACE_UUID,
        "group_id": null,
        "future_additive_field": {"ignored": true}
    }))
    .unwrap();

    let parsed = parse_created_workspace(&bytes).expect("live cmux create shape");
    assert_eq!(parsed.workspace_id, UUID.to_ascii_lowercase());
    assert_eq!(parsed.surface_id, SURFACE_UUID.to_ascii_lowercase());

    let invalid = serde_json::to_vec(&serde_json::json!({
        "workspace_id": "workspace:1",
        "surface_id": SURFACE_UUID,
        "group_id": null
    }))
    .unwrap();
    assert!(parse_created_workspace(&invalid).is_err());
}

#[test]
fn operation_id_is_stable_payload_bound_rfc9562_uuid_v8() {
    let first = request("codex", ProviderWrapperOperationV1::Submit);
    let same = request("codex", ProviderWrapperOperationV1::Submit);
    let mut changed = request("codex", ProviderWrapperOperationV1::Submit);
    changed.delivery_fence.payload_digest = "2".repeat(64);
    changed.delivery_fence.bind_idempotency_key();

    let first_id = workspace_create_operation_id(&first.delivery_fence.idempotency_key);
    assert_eq!(
        first_id,
        workspace_create_operation_id(&same.delivery_fence.idempotency_key)
    );
    assert_ne!(
        first_id,
        workspace_create_operation_id(&changed.delivery_fence.idempotency_key)
    );
    assert_eq!(first_id.len(), 36);
    assert_eq!(&first_id[14..15], "8");
    assert!(matches!(&first_id[19..20], "8" | "9" | "a" | "b"));
    assert_eq!(first_id.matches('-').count(), 4);
}

fn surface_health(surface_ids: &[&str]) -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::json!({
        "window_id": OTHER_WINDOW_UUID,
        "workspace_id": UUID,
        "surfaces": surface_ids.iter().map(|id| serde_json::json!({
            "id": id,
            "in_window": true,
            "index": 0,
            "type": "terminal"
        })).collect::<Vec<_>>()
    }))
}

fn session_evidence(provider: Option<&str>) -> Result<CommandResult, RunnerFailure> {
    successful_json(serde_json::json!({
        "window_id": OTHER_WINDOW_UUID,
        "workspace_id": UUID,
        "pane_id": "423E4567-E89B-12D3-A456-426614174000",
        "surface_id": SURFACE_UUID,
        "cleared": false,
        "restore_record": null,
        "resume_binding": provider.map(|kind| serde_json::json!({
            "checkpoint_id": SESSION_UUID,
            "kind": kind,
            "source": "agent-hook",
            "execution_location": "local",
            "remote_pty_session_id": null,
            "remote_surface_id": null,
            "remote_workspace_id": null,
            "cwd": "/tmp/shipyard-gen43"
        }))
    }))
}

fn request(provider: &str, operation: ProviderWrapperOperationV1) -> ProviderWrapperRequestV1 {
    let mut fence = ProviderDeliveryFenceV1 {
        wake_id: "wake:gen43:1".to_owned(),
        work_item_id: "work:gen43".to_owned(),
        work_generation: 2,
        owner_generation: 1,
        route_ref: "route:gen43".to_owned(),
        payload_digest: "1".repeat(64),
        attempt: 1,
        consumer_epoch: 1,
        consumer_owner_ref: "consumer:m5".to_owned(),
        idempotency_key: String::new(),
    };
    fence.bind_idempotency_key();
    ProviderWrapperRequestV1 {
        schema_version: PROVIDER_WRAPPER_SCHEMA_VERSION,
        operation,
        delivery_target: ProviderDeliveryTargetV1::FreshCheckpoint,
        provider_id: provider.to_owned(),
        adapter_id: ADAPTER_ID.to_owned(),
        delivery_fence: fence,
        terminal_endpoint: TerminalEndpointV1::Cmux(CmuxEndpointV1 {
            executable_path: "/test/cmux-a".to_owned(),
            socket_path: "/test/cmux-a.sock".to_owned(),
            signing_team_id: "7WLXT3NR37".to_owned(),
        }),
        protected_route: ProtectedProviderRouteV1 {
            argv: vec![
                "/opt/subrouter".to_owned(),
                provider.to_owned(),
                "resume".to_owned(),
                "--model".to_owned(),
                "gpt-5.6-sol".to_owned(),
                "-c".to_owned(),
                "model_reasoning_effort=\"medium\"".to_owned(),
                "native-session-a".to_owned(),
            ],
            fresh_argv: vec![
                "/opt/subrouter".to_owned(),
                provider.to_owned(),
                "--model".to_owned(),
                "gpt-5.6-sol".to_owned(),
                "-c".to_owned(),
                "model_reasoning_effort=\"medium\"".to_owned(),
            ],
            executable_sha256: "9".repeat(64),
            environment: std::collections::BTreeMap::from([
                (
                    format!("SUBROUTER_{}_ACCOUNT_ID", provider.to_ascii_uppercase()),
                    "account-a".to_owned(),
                ),
                (
                    format!("SUBROUTER_{}_USER_EMAIL", provider.to_ascii_uppercase()),
                    "agent@example.test".to_owned(),
                ),
            ]),
            account_id: Some("account-a".to_owned()),
            native_session_id: "native-session-a".to_owned(),
            profile_digest: "1".repeat(64),
        },
        resume_expectation: FreshResumeExpectationV1 {
            workstream_handle: "GEN-43".to_owned(),
            context_url: Some("https://linear.app/generous/issue/GEN-43".to_owned()),
            plan_sha256: "2".repeat(64),
            root_revision: 0,
            issue_revision: 0,
            material_event_revision: 0,
            projection_revision: 1,
            checkpoint_id: "checkpoint-gen43".to_owned(),
            checkpoint_generation: 1,
            checkpoint_digest: "3".repeat(64),
            repository: "generous-corp/shipyard".to_owned(),
            worktree_path: native_absolute_test_path("shipyard-gen43"),
            head_sha: "4".repeat(40),
            expected_resume_context_digest: "5".repeat(64),
            success_continuation_digest: "6".repeat(64),
            failure_continuation_digest: "7".repeat(64),
        },
        launch_options: ProviderLaunchOptionsV1 {
            model_id: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some(ProviderReasoningEffortV1::Medium),
        },
    }
}

fn description(request: &ProviderWrapperRequestV1) -> String {
    format!(
        "shipyard-workstream-delivery:{}",
        request.delivery_fence.idempotency_key
    )
}

fn workspace(description: &str) -> serde_json::Value {
    serde_json::json!({"id": UUID, "description": description, "title": "ignored"})
}

fn workspace_with_id(id: &str, description: &str) -> serde_json::Value {
    serde_json::json!({"id": id, "description": description, "title": "ignored"})
}

fn assert_delivered(response: &ProviderWrapperResponseV1, provider: &str) {
    let ProviderWrapperOutcomeV1::Delivered {
        provider_session_ref,
        receipt_digest,
        ..
    } = &response.outcome
    else {
        panic!("expected delivered, got {:?}", response.outcome);
    };
    assert_eq!(
        provider_session_ref,
        &format!("session:{provider}:{}", SESSION_UUID.to_ascii_lowercase())
    );
    assert_eq!(receipt_digest.len(), 64);
}

#[test]
fn live_original_session_is_woken_in_place_without_workspace_or_provider_creation() {
    let mut request = request("codex", ProviderWrapperOperationV1::Submit);
    request.delivery_target = ProviderDeliveryTargetV1::OriginalSession {
        surface_id: SURFACE_UUID.to_owned(),
    };
    request.protected_route.native_session_id = SESSION_UUID.to_owned();
    *request.protected_route.argv.last_mut().unwrap() = SESSION_UUID.to_owned();
    let mut runner = FakeRunner {
        results: VecDeque::from([
            session_evidence(Some("codex")),
            successful_json(serde_json::json!({})),
            successful_json(serde_json::json!({})),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 3);
    assert_eq!(runner.calls[0][3..6], ["surface", "resume", "show"]);
    assert_eq!(runner.calls[1][0], "send");
    assert_eq!(runner.calls[2][0], "send-key");
    assert!(runner.private_launches.is_empty());
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
    }));
}

#[test]
fn live_original_send_transport_ambiguity_never_becomes_redispatchable() {
    let mut request = request("codex", ProviderWrapperOperationV1::Submit);
    request.delivery_target = ProviderDeliveryTargetV1::OriginalSession {
        surface_id: SURFACE_UUID.to_owned(),
    };
    request.protected_route.native_session_id = SESSION_UUID.to_owned();
    *request.protected_route.argv.last_mut().unwrap() = SESSION_UUID.to_owned();
    let mut runner = FakeRunner {
        results: VecDeque::from([
            session_evidence(Some("codex")),
            Err(RunnerFailure::Unavailable),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert!(runner.private_launches.is_empty());
    assert_eq!(runner.calls.len(), 2);
}

#[test]
fn live_original_refuses_remote_binding_at_final_pre_send_check() {
    let mut request = request("codex", ProviderWrapperOperationV1::Submit);
    request.delivery_target = ProviderDeliveryTargetV1::OriginalSession {
        surface_id: SURFACE_UUID.to_owned(),
    };
    request.protected_route.native_session_id = SESSION_UUID.to_owned();
    *request.protected_route.argv.last_mut().unwrap() = SESSION_UUID.to_owned();
    let mut remote = session_evidence(Some("codex")).unwrap().stdout;
    let mut value: serde_json::Value = serde_json::from_slice(&remote).unwrap();
    value["resume_binding"]["execution_location"] = serde_json::json!("remote");
    value["resume_binding"]["remote_surface_id"] = serde_json::json!(SURFACE_UUID);
    remote = serde_json::to_vec(&value).unwrap();
    let mut runner = FakeRunner {
        results: VecDeque::from([Ok(CommandResult {
            success: true,
            stdout: remote,
        })]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert_eq!(runner.calls.len(), 1);
    assert!(runner.private_launches.is_empty());
}

#[test]
fn exact_replay_returns_existing_workspace_without_create() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 4);
    assert_eq!(runner.calls[0], cmux_prefix(["list-windows"]));
    assert_eq!(runner.calls[1][3..5], ["workspace", "list"]);
}

#[test]
fn cross_window_exact_description_duplicates_refuse_without_create() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let exact = description(&request);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID, OTHER_WINDOW_UUID]),
            list(serde_json::json!([workspace(&exact)])),
            list_for_window(
                OTHER_WINDOW_UUID,
                serde_json::json!([workspace_with_id(SURFACE_UUID, &exact)]),
            ),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 3);
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
    }));
}

#[test]
fn workspace_created_before_agent_hook_session_is_not_accepted() {
    let request = request("codex", ProviderWrapperOperationV1::Reconcile);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(None),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 4);
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
    }));
}

#[test]
fn missing_raw_create_capability_refuses_before_private_launch_or_mutation() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            successful_json(serde_json::json!({
                "protocol": "cmux-socket",
                "version": 2,
                "methods": ["workspace.create"],
                "capabilities": []
            })),
        ]),
        ..FakeRunner::default()
    };
    let mut provider = FakeProviderLaunchAuthority::default();

    let response = handle_request(&request, &mut runner, &mut provider);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert_eq!(provider.verify_calls, 1);
    assert_eq!(provider.prepare_calls, 0);
    assert_eq!(runner.calls[2][3..5], ["rpc", "system.capabilities"]);
    assert!(runner.private_launches.is_empty());
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
    }));
}

#[test]
fn post_create_window_and_workspace_fence_mismatch_is_uncertain() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            created(),
            windows(&[OTHER_WINDOW_UUID]),
            list_for_window(
                OTHER_WINDOW_UUID,
                serde_json::json!([workspace(&description(&request))]),
            ),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 6);
    assert_eq!(runner.private_launches.len(), 1);
}

#[test]
fn post_create_returned_workspace_id_mismatch_is_uncertain() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            created(),
            windows(&[UUID]),
            list(serde_json::json!([workspace_with_id(
                OTHER_WINDOW_UUID,
                &description(&request),
            )])),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 6);
}

#[test]
fn sole_provider_binding_on_different_returned_surface_is_uncertain() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut other_evidence = session_evidence(Some("codex")).unwrap().stdout;
    let mut value: serde_json::Value = serde_json::from_slice(&other_evidence).unwrap();
    value["surface_id"] = serde_json::json!(OTHER_WINDOW_UUID);
    other_evidence = serde_json::to_vec(&value).unwrap();
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            created(),
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[OTHER_WINDOW_UUID]),
            Ok(CommandResult {
                success: true,
                stdout: other_evidence,
            }),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 8);
}

#[test]
fn matching_provider_with_non_agent_hook_source_is_uncertain() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut untrusted_evidence = session_evidence(Some("codex")).unwrap().stdout;
    let mut value: serde_json::Value = serde_json::from_slice(&untrusted_evidence).unwrap();
    value["resume_binding"]["source"] = serde_json::json!("manual");
    untrusted_evidence = serde_json::to_vec(&value).unwrap();
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            created(),
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            Ok(CommandResult {
                success: true,
                stdout: untrusted_evidence,
            }),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 8);
}

#[test]
fn post_create_multiple_agent_bindings_are_never_delivered() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut second_evidence = session_evidence(Some("codex")).unwrap().stdout;
    let mut second_value: serde_json::Value = serde_json::from_slice(&second_evidence).unwrap();
    second_value["surface_id"] = serde_json::json!(OTHER_WINDOW_UUID);
    second_evidence = serde_json::to_vec(&second_value).unwrap();
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            created(),
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID, OTHER_WINDOW_UUID]),
            session_evidence(Some("codex")),
            Ok(CommandResult {
                success: true,
                stdout: second_evidence,
            }),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_with_default_provider(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 9);
}

#[test]
fn reconciliation_of_existing_session_does_not_require_launch_executable() {
    let request = request("codex", ProviderWrapperOperationV1::Reconcile);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let mut provider = FakeProviderLaunchAuthority {
        route_verification: Some(Err("subrouter-executable-drift")),
        ..FakeProviderLaunchAuthority::default()
    };

    let response = handle_request(&request, &mut runner, &mut provider);

    assert_delivered(&response, "codex");
    assert!(provider.route_verification.is_some());
    assert_eq!(provider.verify_calls, 0);
    assert_eq!(provider.prepare_calls, 0);
}

#[test]
fn lost_create_response_reconciles_without_second_create() {
    let submit = request("codex", ProviderWrapperOperationV1::Submit);
    let mut submit_runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            Err(RunnerFailure::Unavailable),
        ]),
        ..FakeRunner::default()
    };
    let submit_response = handle_with_default_provider(&submit, &mut submit_runner);
    assert!(matches!(
        submit_response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(submit_runner.calls.len(), 4);
    assert_eq!(submit_runner.calls[3][3..5], ["rpc", "workspace.create"]);

    let mut reconcile = submit.clone();
    reconcile.operation = ProviderWrapperOperationV1::Reconcile;
    let mut reconcile_runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID, OTHER_WINDOW_UUID]),
            list(serde_json::json!([])),
            list_for_window(
                OTHER_WINDOW_UUID,
                serde_json::json!([workspace(&description(&reconcile))]),
            ),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let reconciled = handle_with_default_provider(&reconcile, &mut reconcile_runner);
    assert_delivered(&reconciled, "codex");
    assert_eq!(reconcile_runner.calls.len(), 5);
    assert!(reconcile_runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
    }));
}

#[test]
fn reconcile_with_no_match_never_creates() {
    let request = request("codex", ProviderWrapperOperationV1::Reconcile);
    let mut runner = FakeRunner {
        results: VecDeque::from([windows(&[UUID]), list(serde_json::json!([]))]),
        ..FakeRunner::default()
    };
    let response = handle_with_default_provider(&request, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 2);
}

#[test]
fn delayed_workspace_visibility_keeps_one_fence_and_never_creates_again() {
    let submit = request("codex", ProviderWrapperOperationV1::Submit);
    let original_key = submit.delivery_fence.idempotency_key.clone();
    let mut submit_runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            Err(RunnerFailure::Unavailable),
        ]),
        ..FakeRunner::default()
    };
    let submitted = handle_with_default_provider(&submit, &mut submit_runner);
    assert!(matches!(
        submitted.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(submitted.idempotency_key, original_key);

    let mut reconcile = submit.clone();
    reconcile.operation = ProviderWrapperOperationV1::Reconcile;
    let mut hidden_runner = FakeRunner {
        results: VecDeque::from([windows(&[UUID]), list(serde_json::json!([]))]),
        ..FakeRunner::default()
    };
    let hidden = handle_with_default_provider(&reconcile, &mut hidden_runner);
    assert!(matches!(
        hidden.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(hidden.idempotency_key, original_key);

    let mut visible_runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&reconcile))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let visible = handle_with_default_provider(&reconcile, &mut visible_runner);
    assert_delivered(&visible, "codex");
    assert_eq!(visible.idempotency_key, original_key);
    assert_eq!(reconcile.delivery_fence.idempotency_key, original_key);
    let create_count = submit_runner
        .calls
        .iter()
        .chain(&hidden_runner.calls)
        .chain(&visible_runner.calls)
        .filter(|call| {
            call.get(3..5) == Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
        })
        .count();
    assert_eq!(create_count, 1);
}

#[test]
fn same_title_with_wrong_description_does_not_replay() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(
                "shipyard-workstream-delivery:wrong"
            )])),
            workspace_create_capabilities(),
            created(),
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let response = handle_with_default_provider(&request, &mut runner);
    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 8);
    let create = &runner.calls[3];
    assert_eq!(create[3..5], ["rpc", "workspace.create"]);
    let params: serde_json::Value = serde_json::from_str(&create[5]).unwrap();
    assert_eq!(params["description"], description(&request));
}

#[test]
fn structured_launch_quotes_cwd_and_excludes_raw_context() {
    let mut request = request("codex", ProviderWrapperOperationV1::Submit);
    let quoted_worktree = native_absolute_test_path("work tree'quoted");
    request.resume_expectation.worktree_path = quoted_worktree.clone();
    request.resume_expectation.context_url =
        Some("https://linear.app/generous/private-secret'raw".to_owned());
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            workspace_create_capabilities(),
            created(),
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let response = handle_with_default_provider(&request, &mut runner);
    assert_delivered(&response, "codex");
    let create = &runner.calls[3];
    assert_eq!(create[..3], ["--json", "--id-format", "uuids"]);
    assert_eq!(create[3..5], ["rpc", "workspace.create"]);
    let params: serde_json::Value = serde_json::from_str(&create[5]).unwrap();
    assert_eq!(params["cwd"], quoted_worktree);
    assert_eq!(params["description"], description(&request));
    assert_eq!(params["title"], "GEN-43 — tracked workstream");
    assert_eq!(params["focus"], false);
    assert_eq!(params["eager_load_terminal"], true);
    assert_eq!(params.as_object().unwrap().len(), 7);
    assert_eq!(
        params["operation_id"],
        workspace_create_operation_id(&request.delivery_fence.idempotency_key)
    );
    let command = params["initial_command"].as_str().unwrap();
    let launch = &runner.private_launches[0];
    assert!(!private_launch_path(command).unwrap().exists());
    assert!(!command.contains("private-secret"));
    assert!(!command.contains("context_url"));
    assert!(!command.contains("GEN-43"));
    assert!(!command.contains("account-a"));
    assert!(!command.contains("agent@example.test"));
    assert!(launch.contains("GEN-43"));
    assert!(launch.contains("wake:gen43:1"));
    for command_name in [
        "context-challenge",
        "acknowledge-context",
        "return-challenge",
        "return-ownership",
    ] {
        assert!(launch.contains(command_name));
    }
    assert!(launch.contains("Never put receipt JSON or secrets in argv"));
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["rpc".to_owned(), "surface.send_text".to_owned()].as_slice())
            && call.first().map(String::as_str) != Some("send")
    }));
}

#[cfg(unix)]
mod ledger;
mod route_and_policy;
