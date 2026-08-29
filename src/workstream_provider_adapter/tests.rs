use super::*;
use crate::provider_wrapper::{
    FreshResumeExpectationV1, ProviderDeliveryFenceV1, ProviderLaunchOptionsV1,
    ProviderSecretFileV1, SubrouterRoutingV1,
};

fn digest(value: &[u8]) -> String {
    hex::encode(Sha256::digest(value))
}

fn request(operation: ProviderWrapperOperationV1) -> ProviderWrapperRequestV1 {
    let mut fence = ProviderDeliveryFenceV1 {
        ledger_incarnation_ref: "ledger-1".to_owned(),
        dispatcher_epoch_ref: "dispatcher-1".to_owned(),
        wake_id: "wake-1".to_owned(),
        claim_id: "claim-1".to_owned(),
        work_item_id: "work-1".to_owned(),
        work_generation: 7,
        owner_generation: 3,
        route_ref: "route-1".to_owned(),
        payload_digest: digest(b"payload"),
        attempt: 1,
        claimant_ref: "machine-1".to_owned(),
        idempotency_key: String::new(),
    };
    fence.bind_idempotency_key();
    ProviderWrapperRequestV1 {
        schema_version: 1,
        operation,
        provider_id: "codex".to_owned(),
        adapter_id: "subrouter".to_owned(),
        delivery_fence: fence,
        subrouter_routing: SubrouterRoutingV1 {
            terminal: ProviderTerminalRouteV1::Cmux {
                workspace_ref: "workspace-original".to_owned(),
                pane_ref: "pane-original".to_owned(),
                surface_ref: "surface-original".to_owned(),
            },
            native_session_ref:
                "opaque:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
            native_resume_ref:
                "opaque:sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
            native_resume_id: "cccccccc-cccc-4ccc-8ccc-cccccccccccc".to_owned(),
            server_ref: "server-1".to_owned(),
            provider_route_ref: "provider-route-1".to_owned(),
            account_ref: "account-ref-1".to_owned(),
            account_file: ProviderSecretFileV1 {
                path: "/Users/test/.config/pulp/secrets/subrouter-account".to_owned(),
                sha256: digest(b"private-account"),
            },
            model_ref: "model-ref-1".to_owned(),
            wrapper_ref: "wrapper-ref-1".to_owned(),
            companion_sha256: digest(b"companion"),
            subrouter_executable_path: "/Users/test/.local/bin/subrouter".to_owned(),
            subrouter_executable_sha256: digest(b"subrouter"),
            agent_executable_path: "/Users/test/.local/bin/codex".to_owned(),
            agent_executable_sha256: digest(b"codex"),
            session_headers_ref: "headers-ref-1".to_owned(),
            session_headers_file: ProviderSecretFileV1 {
                path: "/Users/test/.config/pulp/secrets/subrouter-headers".to_owned(),
                sha256: digest(b"private-headers"),
            },
            routing_generation: 4,
            launch_generation: 3,
            launch_revision: 2,
            agent_adapter_generation: 5,
            agent_adapter_revision: 6,
        },
        resume_expectation: FreshResumeExpectationV1 {
            workstream_handle: "GEN-14".to_owned(),
            context_url: Some("https://linear.app/generous-corp/issue/GEN-14/status".to_owned()),
            plan_sha256: digest(b"plan"),
            root_revision: 0,
            issue_revision: 0,
            material_event_revision: 0,
            projection_revision: 4,
            checkpoint_id: "checkpoint-1".to_owned(),
            checkpoint_generation: 1,
            checkpoint_digest: digest(b"checkpoint"),
            repository: "danielraffel/shipyard".to_owned(),
            worktree_path: "/Volumes/Workshop/Code/shipyard".to_owned(),
            head_sha: "a".repeat(40),
            expected_resume_context_digest: digest(b"context"),
            success_continuation_digest: digest(b"success"),
            failure_continuation_digest: digest(b"failure"),
        },
        launch_options: ProviderLaunchOptionsV1 {
            model_id: Some("gpt-5.6-sol".to_owned()),
            reasoning_effort: Some(ProviderReasoningEffortV1::Medium),
        },
    }
}

struct ExistingRunner {
    calls: usize,
}

struct EmptyRunner;

impl CmuxRunner for EmptyRunner {
    fn verify(&mut self) -> Result<(), &'static str> {
        Ok(())
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, &'static str> {
        if args.ends_with(&["list-windows".to_owned()]) {
            return Ok(CommandResult {
                success: true,
                stdout: br#"[{"id":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}]"#.to_vec(),
            });
        }
        Ok(CommandResult {
            success: true,
            stdout: serde_json::to_vec(&serde_json::json!({
                "window_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "workspaces": []
            }))
            .unwrap(),
        })
    }
}

impl CmuxRunner for ExistingRunner {
    fn verify(&mut self) -> Result<(), &'static str> {
        Ok(())
    }

    fn run(&mut self, args: &[String]) -> Result<CommandResult, &'static str> {
        self.calls += 1;
        if args.ends_with(&["list-windows".to_owned()]) {
            return Ok(CommandResult {
                success: true,
                stdout: br#"[{"id":"aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"}]"#.to_vec(),
            });
        }
        if args.iter().any(|arg| arg == "surface-health") {
            return Ok(CommandResult {
                success: true,
                stdout: serde_json::to_vec(&serde_json::json!({
                    "workspace_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    "surfaces": [{
                        "id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
                        "type": "terminal"
                    }]
                }))
                .unwrap(),
            });
        }
        if args.iter().any(|arg| arg == "resume") && args.iter().any(|arg| arg == "show") {
            return Ok(CommandResult {
                success: true,
                stdout: serde_json::to_vec(&serde_json::json!({
                    "workspace_id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    "surface_id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
                    "resume_binding": {
                        "checkpoint_id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
                        "kind": "codex",
                        "source": "agent-hook"
                    }
                }))
                .unwrap(),
            });
        }
        let description = format!(
            "shipyard-workstream-delivery:{}",
            request(ProviderWrapperOperationV1::Submit)
                .delivery_fence
                .idempotency_key
        );
        Ok(CommandResult {
            success: true,
            stdout: serde_json::to_vec(&serde_json::json!({
                "window_id": "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa",
                "workspaces": [{
                    "id": "bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb",
                    "description": description
                }]
            }))
            .unwrap(),
        })
    }
}

#[test]
fn reconciliation_finds_exact_workspace_without_launch_or_secret_output() {
    let request = request(ProviderWrapperOperationV1::Reconcile);
    let mut runner = ExistingRunner { calls: 0 };
    let response = handle_request(&request, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Delivered { .. }
    ));
    assert_eq!(runner.calls, 4);
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(!encoded.contains("private-account"));
    assert!(!encoded.contains("private-headers"));
    assert!(!encoded.contains("subrouter-account"));
}

#[test]
fn empty_reconciliation_snapshot_remains_uncertain() {
    let request = request(ProviderWrapperOperationV1::Reconcile);
    let response = handle_request(&request, &mut EmptyRunner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
}

#[test]
fn herdr_is_explicitly_refused_before_any_terminal_call() {
    let mut request = request(ProviderWrapperOperationV1::Submit);
    request.subrouter_routing.terminal = ProviderTerminalRouteV1::HerdR {
        session_ref: "session-1".to_owned(),
        workspace_ref: "workspace-1".to_owned(),
        tab_ref: "tab-1".to_owned(),
        pane_ref: "pane-1".to_owned(),
    };
    let mut runner = ExistingRunner { calls: 0 };
    let response = handle_request(&request, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert_eq!(runner.calls, 0);
}

#[test]
fn herdr_reconciliation_preserves_uncertainty_without_terminal_call() {
    let mut request = request(ProviderWrapperOperationV1::Reconcile);
    request.subrouter_routing.terminal = ProviderTerminalRouteV1::HerdR {
        session_ref: "session-1".to_owned(),
        workspace_ref: "workspace-1".to_owned(),
        tab_ref: "tab-1".to_owned(),
        pane_ref: "pane-1".to_owned(),
    };
    let mut runner = ExistingRunner { calls: 0 };
    let response = handle_request(&request, &mut runner);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Uncertain { .. }
    ));
    assert_eq!(runner.calls, 0);
}

#[test]
fn launch_command_contains_only_reference_paths() {
    let request = request(ProviderWrapperOperationV1::Submit);
    let capsule = Path::new("/Users/test/.config/pulp/secrets/.capsule.json");
    let companion = Path::new("/Applications/Shipyard/shipyard-workstream-provider");
    let args = create_args(&request, "description", capsule, companion).unwrap();
    let encoded = args.join(" ");
    assert!(encoded.contains(".capsule.json"));
    assert!(!encoded.contains("private-account"));
    assert!(!encoded.contains("private-headers"));
}

#[test]
fn pinned_file_requires_exact_mode_and_digest() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("secret");
    fs::write(&path, b"account-1\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        read_pinned_file(&path, Some(&digest(b"account-1\n")), 1024, 0o600, false,).unwrap(),
        b"account-1\n"
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(read_pinned_file(&path, Some(&digest(b"account-1\n")), 1024, 0o600, false,).is_err());
}

#[test]
fn live_worktree_verification_refuses_a_non_repository() {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let mut request = request(ProviderWrapperOperationV1::Submit);
    request.resume_expectation.worktree_path = temp.path().to_string_lossy().into_owned();
    assert!(verify_live_worktree(&request).is_err());
}
