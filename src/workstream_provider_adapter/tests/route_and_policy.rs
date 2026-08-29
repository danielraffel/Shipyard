use super::*;

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

#[test]
fn cmux_transport_choice_is_orthogonal_to_subrouter_provider_route() {
    let request = request("claude", ProviderWrapperOperationV1::Submit);
    let expected_terminal = request.terminal_endpoint.clone();
    let mut terminal = FakeRunner {
        results: VecDeque::from([
            windows(&[UUID]),
            list(serde_json::json!([])),
            created(),
            session_evidence(Some("claude")),
        ]),
        ..FakeRunner::default()
    };
    let mut provider = FakeProviderLaunchAuthority::default();

    let response = handle_request(&request, &mut terminal, &mut provider);

    assert_delivered(&response, "claude");
    assert_eq!(terminal.bound_endpoints, vec![expected_terminal]);
    assert_eq!(provider.verify_calls, 1);
    assert_eq!(provider.prepare_calls, 1);
    assert_eq!(
        provider.verified_routes,
        vec![("/opt/subrouter".to_owned(), "claude".to_owned())]
    );
    assert!(terminal.private_launches[0].contains("exec '/opt/subrouter' 'claude'"));
}

#[test]
fn missing_subrouter_refuses_without_direct_provider_or_terminal_create() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let mut terminal = FakeRunner {
        results: VecDeque::from([windows(&[UUID]), list(serde_json::json!([]))]),
        ..FakeRunner::default()
    };
    let mut provider = FakeProviderLaunchAuthority {
        route_verification: Some(Err("subrouter-executable-unavailable")),
        ..FakeProviderLaunchAuthority::default()
    };

    let response = handle_request(&request, &mut terminal, &mut provider);

    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Rejected { .. }
    ));
    assert_eq!(provider.verify_calls, 1);
    assert_eq!(provider.prepare_calls, 0);
    assert_eq!(terminal.calls.len(), 2);
    assert!(!terminal.calls.iter().flatten().any(|arg| arg == "create"));
    assert!(terminal.private_launches.is_empty());
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
        let response = handle_with_default_provider(&request, &mut runner);
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
    second.terminal_endpoint = TerminalEndpointV1::Cmux(CmuxEndpointV1 {
        executable_path: "/test/cmux-b".into(),
        socket_path: "/test/cmux-b.sock".into(),
        signing_team_id: "ABCDEFGHIJ".into(),
    });
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
        let response = handle_with_default_provider(request, &mut runner);
        assert!(matches!(
            response.outcome,
            ProviderWrapperOutcomeV1::Uncertain { .. }
        ));
    }
    assert_eq!(
        runner.bound_endpoints,
        vec![first.terminal_endpoint, second.terminal_endpoint]
    );
}

#[test]
fn herdr_shape_never_falls_back_to_cmux_even_with_declared_proofs() {
    let mut request = request("codex", ProviderWrapperOperationV1::Submit);
    request.terminal_endpoint = TerminalEndpointV1::HerdR {
        socket_path: "/test/herdr.sock".into(),
        server_incarnation: Some("server-epoch-1".into()),
        direct_fresh_launch_proven: true,
    };
    let mut runner = FakeRunner::default();
    let mut provider = FakeProviderLaunchAuthority::default();
    let response = handle_request(&request, &mut runner, &mut provider);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Rejected { .. }
    ));
    assert!(runner.bound_endpoints.is_empty());
    assert!(runner.calls.is_empty());
}

#[test]
fn request_cannot_select_a_different_cmux_signing_identity() {
    let request = request("codex", ProviderWrapperOperationV1::Submit);
    let TerminalEndpointV1::Cmux(endpoint) = &request.terminal_endpoint else {
        panic!("cmux fixture");
    };
    assert_eq!(
        verify_cmux_signing_policy(endpoint, "ABCDEFGHIJ"),
        Err(RunnerFailure::Untrusted)
    );
    assert_eq!(verify_cmux_signing_policy(endpoint, "7WLXT3NR37"), Ok(()));
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
    let response = handle_with_default_provider(&request, &mut runner);
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
    let before_create = handle_with_default_provider(&request, &mut list_refusal);
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
    let after_create = handle_with_default_provider(&request, &mut create_refusal);
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
    let response = handle_with_default_provider(&request, &mut runner);
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
    let response = handle_with_default_provider(&valid_request, &mut untrusted);
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Rejected { .. }
    ));
    assert!(untrusted.calls.is_empty());

    let mut unsupported = request("other", ProviderWrapperOperationV1::Submit);
    unsupported.protected_route.argv[0] = "/opt/other".into();
    let mut runner = FakeRunner::default();
    let response = handle_with_default_provider(&unsupported, &mut runner);
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
    let response = handle_with_default_provider(
        &request,
        &mut ProductionCmuxRunner::new("7WLXT3NR37".to_owned()),
    );
    assert!(matches!(
        response.outcome,
        ProviderWrapperOutcomeV1::Retryable { .. }
    ));
    assert!(runner.bound_endpoints.is_empty());
    assert!(runner.calls.is_empty());
    assert_eq!(provider.verify_calls, 0);
    assert_eq!(provider.prepare_calls, 0);
}

#[test]
fn reconcile_preflight_failures_preserve_uncertainty() {
    let reconcile = request("codex", ProviderWrapperOperationV1::Reconcile);
    for verification in [RunnerFailure::Unavailable, RunnerFailure::Untrusted] {
        let mut runner = FakeRunner {
            verification: Some(Err(verification)),
            ..FakeRunner::default()
        };
        let response = handle_with_default_provider(&reconcile, &mut runner);
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
    let response = handle_with_default_provider(&unsupported, &mut runner);
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
