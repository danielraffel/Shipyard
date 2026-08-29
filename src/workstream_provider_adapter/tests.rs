#![allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]

use std::collections::VecDeque;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

use super::*;
use crate::provider_wrapper::{
    CmuxEndpointV1, FreshResumeExpectationV1, ProtectedProviderRouteV1, ProviderDeliveryFenceV1,
    ProviderLaunchOptionsV1, ProviderReasoningEffortV1,
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
    subrouter_verification: Option<Result<(), &'static str>>,
    verification: Option<Result<(), RunnerFailure>>,
    bound_endpoints: Vec<CmuxEndpointV1>,
    results: VecDeque<Result<CommandResult, RunnerFailure>>,
    calls: Vec<Vec<String>>,
    private_launches: Vec<String>,
}

fn private_launch_path(command: &str) -> Option<PathBuf> {
    Some(PathBuf::from(
        command.strip_prefix("'/bin/sh' '")?.strip_suffix('\'')?,
    ))
}

impl CmuxRunner for FakeRunner {
    fn verify_subrouter(
        &mut self,
        _request: &ProviderWrapperRequestV1,
    ) -> Result<(), &'static str> {
        self.subrouter_verification.take().unwrap_or(Ok(()))
    }

    fn prepare_private_launch(
        &mut self,
        request: &ProviderWrapperRequestV1,
    ) -> Result<PrivateLaunch, &'static str> {
        prepare_private_launch(request, false)
    }

    fn bind(&mut self, endpoint: &CmuxEndpointV1) -> Result<(), RunnerFailure> {
        self.bound_endpoints.push(endpoint.clone());
        self.verification.take().unwrap_or(Ok(()))
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure> {
        self.calls.push(args.to_vec());
        let result = self
            .results
            .pop_front()
            .expect("test runner must provide one result per call");
        if let Some(index) = args.iter().position(|argument| argument == "--command")
            && let Some(path) = private_launch_path(&args[index + 1])
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
        schema_version: 1,
        operation,
        delivery_target: ProviderDeliveryTargetV1::FreshCheckpoint,
        provider_id: provider.to_owned(),
        adapter_id: ADAPTER_ID.to_owned(),
        delivery_fence: fence,
        cmux_endpoint: CmuxEndpointV1 {
            executable_path: "/test/cmux-a".to_owned(),
            socket_path: "/test/cmux-a.sock".to_owned(),
        },
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

    let response = handle_request(&request, &mut runner);

    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 3);
    assert_eq!(runner.calls[0][3..6], ["surface", "resume", "show"]);
    assert_eq!(runner.calls[1][0], "send");
    assert_eq!(runner.calls[2][0], "send-key");
    assert!(runner.private_launches.is_empty());
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["workspace".to_owned(), "create".to_owned()].as_slice())
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

    let response = handle_request(&request, &mut runner);

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

    let response = handle_request(&request, &mut runner);

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

    let response = handle_request(&request, &mut runner);

    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 4);
    assert_eq!(runner.calls[0], cmux_prefix(["list-windows"]));
    assert_eq!(runner.calls[1][3..5], ["workspace", "list"]);
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

    let response = handle_request(&request, &mut runner);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 4);
    assert!(runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["workspace".to_owned(), "create".to_owned()].as_slice())
    }));
}

#[test]
fn reconciliation_of_existing_session_does_not_require_launch_executable() {
    let request = request("codex", ProviderWrapperOperationV1::Reconcile);
    let mut runner = FakeRunner {
        subrouter_verification: Some(Err("subrouter-executable-drift")),
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
            surface_health(&[SURFACE_UUID]),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_request(&request, &mut runner);

    assert_delivered(&response, "codex");
    assert!(runner.subrouter_verification.is_some());
}

#[test]
fn lost_create_response_reconciles_without_second_create() {
    let submit = request("codex", ProviderWrapperOperationV1::Submit);
    let mut submit_runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            Err(RunnerFailure::Unavailable),
        ]),
        ..FakeRunner::default()
    };
    let submit_response = handle_request(&submit, &mut submit_runner);
    assert!(matches!(
        submit_response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(submit_runner.calls.len(), 3);
    assert_eq!(submit_runner.calls[2][3..5], ["workspace", "create"]);

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
    let reconciled = handle_request(&reconcile, &mut reconcile_runner);
    assert_delivered(&reconciled, "codex");
    assert_eq!(reconcile_runner.calls.len(), 5);
    assert!(reconcile_runner.calls.iter().all(|call| {
        call.get(3..5) != Some(["workspace".to_owned(), "create".to_owned()].as_slice())
    }));
}

#[test]
fn reconcile_with_no_match_never_creates() {
    let request = request("codex", ProviderWrapperOperationV1::Reconcile);
    let mut runner = FakeRunner {
        results: VecDeque::from([windows(&[UUID]), list(serde_json::json!([]))]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
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
            Err(RunnerFailure::Unavailable),
        ]),
        ..FakeRunner::default()
    };
    let submitted = handle_request(&submit, &mut submit_runner);
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
    let hidden = handle_request(&reconcile, &mut hidden_runner);
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
    let visible = handle_request(&reconcile, &mut visible_runner);
    assert_delivered(&visible, "codex");
    assert_eq!(visible.idempotency_key, original_key);
    assert_eq!(reconcile.delivery_fence.idempotency_key, original_key);
    let create_count = submit_runner
        .calls
        .iter()
        .chain(&hidden_runner.calls)
        .chain(&visible_runner.calls)
        .filter(|call| {
            call.get(3..5) == Some(["workspace".to_owned(), "create".to_owned()].as_slice())
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
            created(),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 4);
    let create = &runner.calls[2];
    assert_eq!(create[3..5], ["workspace", "create"]);
    let description_index = create
        .iter()
        .position(|arg| arg == "--description")
        .unwrap();
    assert_eq!(create[description_index + 1], description(&request));
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
            created(),
            session_evidence(Some("codex")),
        ]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
    assert_delivered(&response, "codex");
    let create = &runner.calls[2];
    assert_eq!(create[..3], ["--json", "--id-format", "uuids"]);
    let cwd_index = create.iter().position(|arg| arg == "--cwd").unwrap();
    assert_eq!(create[cwd_index + 1], quoted_worktree);
    let command_index = create.iter().position(|arg| arg == "--command").unwrap();
    let command = &create[command_index + 1];
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
}

#[test]
fn exact_protected_subrouter_route_is_executed_without_direct_fallback() {
    let codex = request("codex", ProviderWrapperOperationV1::Submit);
    let codex_body =
        launch_command(&codex, Path::new(&codex.protected_route.argv[0]), false).unwrap();
    assert!(codex_body.starts_with("export 'SUBROUTER_CODEX_ACCOUNT_ID=account-a'\nexport 'SUBROUTER_CODEX_USER_EMAIL=agent@example.test'\nexec '/opt/subrouter' 'codex' '--model'"));
    assert!(!codex_body.contains("'resume'"));
    assert!(!codex_body.contains("'native-session-a'"));
    assert!(!codex_body.contains("cmux-codex-wrapper"));
    let codex_launch = prepare_private_launch(&codex, false).unwrap();
    assert!(!codex_launch.command.contains("account-a"));
    assert!(!codex_launch.command.contains("agent@example.test"));
    assert!(!codex_launch.command.contains("native-session-a"));
    let launch_path = private_launch_path(&codex_launch.command).unwrap();
    let metadata = std::fs::metadata(&launch_path).unwrap();
    #[cfg(unix)]
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(
        std::fs::read_to_string(&launch_path)
            .unwrap()
            .lines()
            .last(),
        codex_body.lines().last()
    );
    drop(codex_launch);
    assert!(!launch_path.exists());
    let public_response = serde_json::to_string(&response(&codex, rejected("test"))).unwrap();
    for private in [
        "account-a",
        "agent@example.test",
        "native-session-a",
        "/opt/subrouter",
    ] {
        assert!(!public_response.contains(private));
    }

    let mut claude = request("claude", ProviderWrapperOperationV1::Submit);
    claude.launch_options.model_id = Some("fable".to_owned());
    claude.launch_options.reasoning_effort = Some(ProviderReasoningEffortV1::High);
    claude.protected_route.argv = vec![
        "/opt/subrouter".into(),
        "claude".into(),
        "--model".into(),
        "fable".into(),
        "--effort".into(),
        "high".into(),
        "--resume".into(),
        "native-session-a".into(),
    ];
    claude.protected_route.fresh_argv = vec![
        "/opt/subrouter".into(),
        "claude".into(),
        "--model".into(),
        "fable".into(),
        "--effort".into(),
        "high".into(),
    ];
    let claude_body =
        launch_command(&claude, Path::new(&claude.protected_route.argv[0]), false).unwrap();
    assert!(claude_body.contains("exec '/opt/subrouter' 'claude' '--model' 'fable'"));
    assert!(!claude_body.contains("cmux-claude-wrapper"));
}

#[cfg(unix)]
#[test]
fn private_launch_capsule_sets_route_environment_and_deletes_itself() {
    let scope = tempfile::tempdir().unwrap();
    let subrouter = scope.path().join("subrouter");
    let output = scope.path().join("observed.txt");
    std::fs::write(
        &subrouter,
        "#!/bin/sh\nprintf '%s\\n' \"$SUBROUTER_QWEN_ACCOUNT_ID\" \"$@\" > \"$SUBROUTER_TEST_OUTPUT\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&subrouter, std::fs::Permissions::from_mode(0o700)).unwrap();

    let mut request = request("qwen", ProviderWrapperOperationV1::Submit);
    request.protected_route.argv[0] = subrouter.to_string_lossy().into_owned();
    request.protected_route.environment = std::collections::BTreeMap::from([
        (
            "SUBROUTER_QWEN_ACCOUNT_ID".to_owned(),
            "account-a".to_owned(),
        ),
        (
            "SUBROUTER_TEST_OUTPUT".to_owned(),
            output.to_string_lossy().into_owned(),
        ),
    ]);
    request.protected_route.executable_sha256 =
        hex::encode(Sha256::digest(std::fs::read(&subrouter).unwrap()));
    let private_launch = prepare_private_launch(&request, true).unwrap();
    let launch_path = private_launch_path(&private_launch.command).unwrap();
    let launch_directory = launch_path.parent().unwrap().to_path_buf();
    let launch_script = std::fs::read_to_string(&launch_path).unwrap();
    assert!(launch_script.contains("trap cleanup EXIT HUP INT TERM"));
    assert!(launch_script.contains("provider_status=$?"));
    assert!(!launch_script.contains("exec '/"));
    assert!(
        std::process::Command::new("/bin/sh")
            .args(["-c", &private_launch.command])
            .status()
            .unwrap()
            .success()
    );
    assert!(!launch_path.exists());
    drop(private_launch);
    assert!(!launch_directory.exists());
    let observed = std::fs::read_to_string(output).unwrap();
    assert!(observed.starts_with("account-a\nqwen\n--model\n"));
    assert!(!observed.contains("native-session-a"));
    assert!(observed.contains("Resume tracked workstream GEN-43"));
}

#[cfg(unix)]
#[test]
fn subrouter_executable_is_digest_pinned_and_permission_checked() {
    let scope = tempfile::tempdir().unwrap();
    let executable = scope.path().join("subrouter");
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut request = request("qwen", ProviderWrapperOperationV1::Submit);
    request.protected_route.argv[0] = executable.to_string_lossy().into_owned();
    request.protected_route.executable_sha256 =
        hex::encode(Sha256::digest(std::fs::read(&executable).unwrap()));
    verify_subrouter_executable(&request).expect("exact executable");

    std::fs::write(&executable, b"#!/bin/sh\nexit 1\n").unwrap();
    assert_eq!(
        verify_subrouter_executable(&request),
        Err("subrouter-executable-drift")
    );
    request.protected_route.executable_sha256 =
        hex::encode(Sha256::digest(std::fs::read(&executable).unwrap()));
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o722)).unwrap();
    assert_eq!(
        verify_subrouter_executable(&request),
        Err("subrouter-executable-untrusted")
    );
}

#[test]
fn profile_route_drift_refuses_before_terminal_enumeration() {
    for mutate in ["digest", "provider", "wrapper"] {
        let mut request = request("qwen", ProviderWrapperOperationV1::Submit);
        match mutate {
            "digest" => request.protected_route.profile_digest = "f".repeat(64),
            "provider" => request.protected_route.argv[1] = "kimi".into(),
            "wrapper" => request.protected_route.argv[0] = "/opt/qwen".into(),
            _ => unreachable!(),
        }
        let mut runner = FakeRunner::default();
        let response = handle_request(&request, &mut runner);
        assert!(matches!(
            response.outcome,
            ProviderWrapperOutcomeV1::Rejected { .. }
        ));
        assert!(runner.calls.is_empty());
        assert!(runner.bound_endpoints.is_empty());
    }
}

#[test]
fn each_request_binds_enumeration_and_mutation_to_its_exact_cmux_endpoint() {
    let first = request("codex", ProviderWrapperOperationV1::Reconcile);
    let mut second = first.clone();
    second.cmux_endpoint = CmuxEndpointV1 {
        executable_path: "/test/cmux-b".into(),
        socket_path: "/test/cmux-b.sock".into(),
    };
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            windows(&[UUID]),
            list(serde_json::json!([])),
        ]),
        ..FakeRunner::default()
    };
    for request in [&first, &second] {
        let response = handle_request(request, &mut runner);
        assert!(matches!(
            response.outcome,
            ProviderWrapperOutcomeV1::Uncertain { .. }
        ));
    }
    assert_eq!(
        runner.bound_endpoints,
        vec![first.cmux_endpoint, second.cmux_endpoint]
    );
}

#[test]
fn ambiguous_or_duplicate_workspace_evidence_is_never_delivered() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let exact = workspace(&description(&request));
    let mut duplicate = exact.clone();
    duplicate["id"] = serde_json::json!("323E4567-E89B-12D3-A456-426614174000");
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([exact, duplicate])),
        ]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls.len(), 2);
}

#[test]
fn pre_create_refusal_is_retryable_but_post_create_refusal_is_uncertain() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut list_refusal = FakeRunner {
        results: VecDeque::from([Ok(CommandResult {
            success: false,
            stdout: Vec::new(),
        })]),
        ..FakeRunner::default()
    };
    let before_create = handle_request(&request, &mut list_refusal);
    assert!(matches!(
        before_create.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert_eq!(list_refusal.calls.len(), 1);

    let mut create_refusal = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            Ok(CommandResult {
                success: false,
                stdout: Vec::new(),
            }),
        ]),
        ..FakeRunner::default()
    };
    let after_create = handle_request(&request, &mut create_refusal);
    assert!(matches!(
        after_create.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(create_refusal.calls.len(), 3);
    let create = &create_refusal.calls[2];
    let command = &create[create.iter().position(|arg| arg == "--command").unwrap() + 1];
    assert!(!private_launch_path(command).unwrap().exists());
}

#[test]
fn partial_app_wide_enumeration_refuses_create() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID, OTHER_WINDOW_UUID]),
            list(serde_json::json!([])),
            Err(RunnerFailure::Unavailable),
        ]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert_eq!(runner.calls.len(), 3);
    assert!(
        !runner
            .calls
            .iter()
            .flatten()
            .any(|argument| argument == "create")
    );
}

#[test]
fn untrusted_cmux_and_direct_provider_fallback_refuse_before_any_command() {
    let valid_request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut untrusted = FakeRunner {
        verification: Some(Err(RunnerFailure::Untrusted)),
        ..FakeRunner::default()
    };
    let response = handle_request(&valid_request, &mut untrusted);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Rejected { .. }
    ));
    assert!(untrusted.calls.is_empty());

    let mut unsupported = request("other", ProviderWrapperOperationV1::Submit);
    unsupported.protected_route.argv[0] = "/opt/other".into();
    let mut runner = FakeRunner::default();
    let response = handle_request(&unsupported, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Rejected { .. }
    ));
    assert!(runner.calls.is_empty());
}

#[cfg(not(target_os = "macos"))]
#[test]
fn production_cmux_adapter_refuses_before_running_any_cmux_command() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let response = handle_request(&request, &mut ProductionCmuxRunner::default());
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
}

#[test]
fn reconcile_preflight_failures_preserve_uncertainty() {
    let reconcile = request("codex", ProviderWrapperOperationV1::Reconcile);
    for verification in [RunnerFailure::Unavailable, RunnerFailure::Untrusted] {
        let mut runner = FakeRunner {
            verification: Some(Err(verification)),
            ..FakeRunner::default()
        };
        let response = handle_request(&reconcile, &mut runner);
        assert!(matches!(
            response.outcome,
            ProviderWrapperOutcomeV1::Uncertain { .. }
        ));
        assert_eq!(
            response.idempotency_key,
            reconcile.delivery_fence.idempotency_key
        );
        assert!(runner.calls.is_empty());
    }

    let mut unsupported = request("other", ProviderWrapperOperationV1::Reconcile);
    unsupported.protected_route.argv[0] = "/opt/direct-provider".into();
    let mut runner = FakeRunner::default();
    let response = handle_request(&unsupported, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(
        response.idempotency_key,
        unsupported.delivery_fence.idempotency_key
    );
    assert!(runner.calls.is_empty());
}

#[derive(Clone)]
struct LedgerProfile {
    digest: String,
    bytes: Vec<u8>,
}

impl FreshAgentLaunchProfile for LedgerProfile {
    fn provider_id(&self) -> &'static str {
        "codex"
    }

    fn provider_launch_options(&self) -> FreshAgentProviderLaunchOptions {
        FreshAgentProviderLaunchOptions {
            model_id: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some(ProviderReasoningEffortV1::Medium),
        }
    }

    fn profile_digest(&self) -> crate::work_ledger::WorkLedgerResult<String> {
        Ok(self.digest.clone())
    }

    fn permits_fresh_agent(&self) -> bool {
        true
    }

    fn protected_profile_bytes(&self) -> crate::work_ledger::WorkLedgerResult<Vec<u8>> {
        Ok(self.bytes.clone())
    }

    fn resume_expectation(&self) -> Option<FreshAgentResumeExpectation<'_>> {
        Some(FreshAgentResumeExpectation {
            workstream_handle: "GEN-43",
            context_url: Some("https://linear.example/GEN-43"),
            plan_sha256: "2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f2f",
            root_revision: 0,
            issue_revision: 0,
            projection_revision: 1,
            material_event_revision: 0,
            checkpoint_id: "checkpoint-gen43",
            checkpoint_generation: 1,
            checkpoint_digest: "3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f3f",
            repository: "generous-corp/shipyard",
            head_sha: "4444444444444444444444444444444444444444",
            expected_resume_context_digest: "5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f",
            success_continuation_digest: "6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f6f",
            failure_continuation_digest: "7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f7f",
        })
    }
}

struct LedgerResolver(LedgerProfile);

impl WakeProfileResolver for LedgerResolver {
    type Profile = LedgerProfile;

    fn resolve(
        &mut self,
        _wake: &WakeEnvelope,
    ) -> crate::work_ledger::WorkLedgerResult<Self::Profile> {
        Ok(self.0.clone())
    }
}

#[derive(Default)]
struct LedgerCmuxAdapter {
    launch_fence: Option<DeliveryFence>,
    reconcile_fence: Option<DeliveryFence>,
    wrapper_keys: Vec<String>,
    create_count: usize,
}

impl LedgerCmuxAdapter {
    fn wrapper_request(
        fence: &DeliveryFence,
        operation: ProviderWrapperOperationV1,
    ) -> ProviderWrapperRequestV1 {
        let mut request = request("codex", operation);
        request.delivery_fence = ProviderDeliveryFenceV1 {
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
        request.delivery_fence.bind_idempotency_key();
        request.protected_route.profile_digest = fence.payload_digest.clone();
        request
    }

    fn map(response: ProviderWrapperResponseV1) -> ProviderOutcome {
        match response.outcome {
            ProviderWrapperOutcomeV1::Delivered { .. } => ProviderOutcome::Delivered {
                receipt: b"delivered".to_vec(),
            },
            ProviderWrapperOutcomeV1::Retryable { .. } => ProviderOutcome::Retryable {
                evidence: b"retryable".to_vec(),
            },
            ProviderWrapperOutcomeV1::Uncertain { .. } => ProviderOutcome::Uncertain {
                evidence: b"uncertain".to_vec(),
            },
            ProviderWrapperOutcomeV1::Rejected { .. } => ProviderOutcome::Rejected {
                evidence: b"rejected".to_vec(),
            },
        }
    }
}

impl ProviderAdapter for LedgerCmuxAdapter {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
        (provider_id == "codex").then(|| ProviderCapability {
            adapter_id: ADAPTER_ID.to_owned(),
            fresh_agent_launch: true,
            idempotent_launch: true,
        })
    }

    fn authorize(
        &mut self,
        fence: &DeliveryFence,
        _operation: ProviderAuthorizationOperation,
    ) -> Result<DeliveryAuthorization, ProviderOutcome> {
        Ok(DeliveryAuthorization::for_test(
            fence.work_generation,
            fence.owner_generation,
        ))
    }

    fn authorize_reconciliation(
        &mut self,
        fence: &DeliveryFence,
    ) -> Result<ReconciliationAuthorization, ProviderOutcome> {
        Ok(ReconciliationAuthorization::for_test(
            crate::work_ledger::reconciliation_fence_digest(fence),
        ))
    }

    fn launch(
        &mut self,
        launch: ProviderLaunchRequest<'_>,
        _authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        self.launch_fence = Some(launch.fence.clone());
        let request = Self::wrapper_request(launch.fence, ProviderWrapperOperationV1::Submit);
        self.wrapper_keys
            .push(request.delivery_fence.idempotency_key.clone());
        let mut runner = FakeRunner {
            results: VecDeque::from([
                windows(&[UUID]),
                list(serde_json::json!([])),
                Err(RunnerFailure::Unavailable),
            ]),
            ..FakeRunner::default()
        };
        let response = handle_request(&request, &mut runner);
        self.create_count += runner
            .calls
            .iter()
            .filter(|call| {
                call.get(3..5) == Some(["workspace".to_owned(), "create".to_owned()].as_slice())
            })
            .count();
        Self::map(response)
    }

    fn reconcile(
        &mut self,
        fence: &DeliveryFence,
        _authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        self.reconcile_fence = Some(fence.clone());
        let request = Self::wrapper_request(fence, ProviderWrapperOperationV1::Reconcile);
        self.wrapper_keys
            .push(request.delivery_fence.idempotency_key.clone());
        let mut runner = FakeRunner {
            verification: Some(Err(RunnerFailure::Unavailable)),
            ..FakeRunner::default()
        };
        let response = handle_request(&request, &mut runner);
        assert!(runner.calls.is_empty());
        Self::map(response)
    }

    fn reconcile_read_only(
        &mut self,
        fence: &DeliveryFence,
        _authority: ReconciliationAuthorization,
    ) -> ProviderOutcome {
        self.reconcile_fence = Some(fence.clone());
        let request = Self::wrapper_request(fence, ProviderWrapperOperationV1::Reconcile);
        self.wrapper_keys
            .push(request.delivery_fence.idempotency_key.clone());
        let mut runner = FakeRunner {
            verification: Some(Err(RunnerFailure::Unavailable)),
            ..FakeRunner::default()
        };
        let response = handle_request(&request, &mut runner);
        assert!(runner.calls.is_empty());
        Self::map(response)
    }
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)]
fn uncertain_submit_then_unavailable_reconcile_survives_reopen_on_one_fence() {
    let temp = tempfile::tempdir().expect("temp");
    let profile_bytes = b"strict-test-profile".to_vec();
    let profile_digest = hex::encode(Sha256::digest(&profile_bytes));
    let policy = crate::workstream_continuation_config::WorkstreamContinuationConfig {
        origin_machine: "m5".to_owned(),
        repositories: vec!["generous-corp/shipyard".to_owned()],
        provider_wrapper: ProviderWrapperConfig {
            executable_path: PathBuf::from(native_absolute_test_path("cmux-provider")),
            executable_sha256: "8".repeat(64),
            provider_id: "codex".to_owned(),
            adapter_id: ADAPTER_ID.to_owned(),
            deadline_seconds: 15,
            max_stdout_bytes: 65_536,
            max_stderr_bytes: 65_536,
        },
    };
    let publication = NativePublicationRequest {
        repository: "generous-corp/shipyard".to_owned(),
        pull_request: 43,
        head_sha: "4".repeat(40),
        base_ref: "main".into(),
        base_sha: "5".repeat(40),
        github_installation_id: 42,
        terminal_authority: crate::terminal_delivery_authority::TerminalCapabilityRequest::Cmux {
            cli_path: "/test/cmux".into(),
            socket_path: "/test/cmux.sock".into(),
            surface_id: "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".into(),
            workspace_id: "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb".into(),
            native_session_id: "session-gen43".into(),
            provider_kind: "codex".into(),
            process: crate::terminal_delivery_authority::LocalProcessIncarnation {
                boot_id: "boot".into(),
                pid: 42,
                start_identity: "start".into(),
            },
        },
        workstream_handle: "GEN-43".to_owned(),
        context_url: Some("https://linear.example/GEN-43".to_owned()),
        origin_machine: "m5".to_owned(),
        owner_id: "owner-gen43".to_owned(),
        owner_generation: 1,
        agent_provider: "codex".to_owned(),
        agent_session_id: "session-gen43".to_owned(),
        route_account: "account-a".into(),
        route_model: "model-a".into(),
        route_wrapper: "subrouter".into(),
        native_resume_digest: "9".repeat(64),
        route_environment_digest: "8".repeat(64),
        route_id: "route-gen43".to_owned(),
        profile_generation: 1,
        profile_revision: 1,
        profile_provider: "codex".to_owned(),
        profile_digest: profile_digest.clone(),
        protected_profile_bytes: profile_bytes.clone(),
        success_continuation_digest: "6".repeat(64),
        failure_continuation_digest: "7".repeat(64),
    };
    let report =
        WorkLedger::plan_or_apply_native_continuation(temp.path(), &publication, &policy, true)
            .expect("publish managed handoff");
    let profile = LedgerProfile {
        digest: profile_digest,
        bytes: profile_bytes,
    };
    let mut resolver = LedgerResolver(profile.clone());
    let mut adapter = LedgerCmuxAdapter::default();
    let ledger = WorkLedger::open_existing(temp.path())
        .expect("open ledger")
        .expect("ledger exists");
    let scheduled = ledger
        .apply_native_steward_disposition(
            &publication.repository,
            publication.pull_request,
            &publication.head_sha,
            crate::work_ledger::NativeStewardDisposition::Actionable,
        )
        .expect("schedule actionable wake");
    assert!(scheduled.wake_enqueued);
    let consumer = WakeConsumerPolicy {
        activation_enabled: true,
        dispatch_enabled: true,
        authorized_repositories: vec!["generous-corp/shipyard".to_owned()],
    };
    assert_eq!(
        ledger
            .consume_one_wake(consumer.clone(), &mut resolver, &mut adapter)
            .expect("uncertain submit"),
        WakeDeliveryResult::Uncertain
    );
    drop(ledger);

    let reopened = WorkLedger::open_existing(temp.path())
        .expect("reopen ledger")
        .expect("ledger exists");
    assert_eq!(
        reopened
            .reconcile_uncertain_wake(&consumer, &report.wake_id, &mut adapter)
            .expect("unavailable reconciliation"),
        WakeDeliveryResult::Uncertain
    );
    drop(reopened);

    let reopened = WorkLedger::open_existing(temp.path())
        .expect("reopen after reconciliation")
        .expect("ledger exists");
    assert_eq!(
        reopened
            .next_uncertain_wake_id(&consumer)
            .expect("uncertain selection"),
        Some(report.wake_id.clone())
    );
    let mut resolver = LedgerResolver(profile);
    assert_eq!(
        reopened
            .consume_one_wake(consumer, &mut resolver, &mut adapter)
            .expect("no pending retry"),
        WakeDeliveryResult::Empty
    );
    let launch_fence = adapter.launch_fence.as_ref().expect("launch fence");
    let reconcile_fence = adapter.reconcile_fence.as_ref().expect("reconcile fence");
    assert_eq!(launch_fence.attempt, 1);
    assert_eq!(reconcile_fence.attempt, 1);
    assert_eq!(
        launch_fence.idempotency_key,
        reconcile_fence.idempotency_key
    );
    assert_eq!(adapter.wrapper_keys.len(), 2);
    assert_eq!(adapter.wrapper_keys[0], adapter.wrapper_keys[1]);
    assert_eq!(adapter.create_count, 1);
}
