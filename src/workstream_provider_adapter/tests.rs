#![allow(clippy::needless_pass_by_value, clippy::unnecessary_wraps)]

use std::collections::VecDeque;

use super::*;
use crate::provider_wrapper::{
    FreshResumeExpectationV1, ProviderDeliveryFenceV1, ProviderLaunchOptionsV1,
};

const UUID: &str = "123E4567-E89B-12D3-A456-426614174000";
const OTHER_WINDOW_UUID: &str = "923E4567-E89B-12D3-A456-426614174000";

#[derive(Default)]
struct FakeRunner {
    verification: Option<Result<(), RunnerFailure>>,
    results: VecDeque<Result<CommandResult, RunnerFailure>>,
    calls: Vec<Vec<String>>,
}

impl CmuxRunner for FakeRunner {
    fn verify(&mut self) -> Result<(), RunnerFailure> {
        self.verification.take().unwrap_or(Ok(()))
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, RunnerFailure> {
        self.calls.push(args.to_vec());
        self.results
            .pop_front()
            .expect("test runner must provide one result per call")
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
        "surface_id": "223E4567-E89B-12D3-A456-426614174000"
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
        provider_id: provider.to_owned(),
        adapter_id: ADAPTER_ID.to_owned(),
        delivery_fence: fence,
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
            worktree_path: "/tmp/shipyard-gen43".to_owned(),
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
        &format!("session:{provider}:{}", UUID.to_ascii_lowercase())
    );
    assert_eq!(receipt_digest.len(), 64);
}

#[test]
fn exact_replay_returns_existing_workspace_without_create() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([workspace(&description(&request))])),
        ]),
        ..FakeRunner::default()
    };

    let response = handle_request(&request, &mut runner);

    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 2);
    assert_eq!(runner.calls[0], cmux_prefix(["list-windows"]));
    assert_eq!(runner.calls[1][3..5], ["workspace", "list"]);
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
        ]),
        ..FakeRunner::default()
    };
    let reconciled = handle_request(&reconcile, &mut reconcile_runner);
    assert_delivered(&reconciled, "codex");
    assert_eq!(reconcile_runner.calls.len(), 3);
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
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert_eq!(runner.calls.len(), 2);
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
        ]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
    assert_delivered(&response, "codex");
    assert_eq!(runner.calls.len(), 3);
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
    request.resume_expectation.worktree_path = "/tmp/work tree'quoted".to_owned();
    request.resume_expectation.context_url =
        Some("https://linear.app/generous/private-secret'raw".to_owned());
    let mut runner = FakeRunner {
        results: VecDeque::from([windows(&[UUID]), list(serde_json::json!([])), created()]),
        ..FakeRunner::default()
    };
    let response = handle_request(&request, &mut runner);
    assert_delivered(&response, "codex");
    let create = &runner.calls[2];
    let cwd_index = create.iter().position(|arg| arg == "--cwd").unwrap();
    assert_eq!(create[cwd_index + 1], "/tmp/work tree'quoted");
    let command_index = create.iter().position(|arg| arg == "--command").unwrap();
    let command = &create[command_index + 1];
    assert!(!command.contains("private-secret"));
    assert!(!command.contains("context_url"));
    assert!(command.contains("GEN-43"));
    assert!(command.contains("wake:gen43:1"));
}

#[test]
fn codex_and_claude_use_provider_owned_command_grammars() {
    let codex = request("codex", ProviderWrapperOperationV1::Submit);
    let codex_command = launch_command(&codex).unwrap();
    assert!(codex_command.starts_with(&shell_word(CODEX_WRAPPER).unwrap()));
    assert!(codex_command.contains("--model 'gpt-5.6-sol'"));
    assert!(codex_command.contains("-c 'model_reasoning_effort=\"medium\"'"));

    let mut claude = request("claude", ProviderWrapperOperationV1::Submit);
    claude.launch_options.model_id = Some("fable".to_owned());
    claude.launch_options.reasoning_effort = Some(ProviderReasoningEffortV1::High);
    let claude_command = launch_command(&claude).unwrap();
    assert!(claude_command.starts_with(&shell_word(CLAUDE_WRAPPER).unwrap()));
    assert!(claude_command.contains("--model 'fable'"));
    assert!(claude_command.contains("--effort high"));
    assert!(!claude_command.contains("model_reasoning_effort"));
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
fn untrusted_cmux_and_unsupported_provider_refuse_before_any_command() {
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

    let unsupported = request("other", ProviderWrapperOperationV1::Submit);
    let mut runner = FakeRunner::default();
    let response = handle_request(&unsupported, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Rejected { .. }
    ));
    assert!(runner.calls.is_empty());
}
