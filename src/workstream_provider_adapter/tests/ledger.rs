use super::*;

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
                workspace_create_capabilities(),
                Err(RunnerFailure::Unavailable),
            ]),
            ..FakeRunner::default()
        };
        let response = handle_with_default_provider(&request, &mut runner);
        self.create_count += runner
            .calls
            .iter()
            .filter(|call| {
                call.get(3..5) == Some(["rpc".to_owned(), "workspace.create".to_owned()].as_slice())
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
        let response = handle_with_default_provider(&request, &mut runner);
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
        let response = handle_with_default_provider(&request, &mut runner);
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
        terminal_trust: Box::new(crate::workstream_continuation_config::TerminalTrustConfig {
            cmux_signing_team_id: "7WLXT3NR37".to_owned(),
        }),
    };
    let publication = NativePublicationRequest {
        repository: "generous-corp/shipyard".to_owned(),
        pull_request: 43,
        head_sha: "4".repeat(40),
        base_ref: "main".into(),
        base_sha: "5".repeat(40),
        github_installation_id: 42,
        repo_policy_revision: 1,
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
        plan_sha256: "7".repeat(64),
        root_revision: 1,
        issue_revision: 1,
        projection_revision: 1,
        material_event_revision: 1,
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
    WorkLedger::open(temp.path())
        .expect("ledger")
        .set_repo_policy(
            &crate::work_ledger::RepoPolicy {
                repo: publication.repository.clone(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: vec!["linux".to_owned(), "windows".to_owned()],
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .expect("repo policy");
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
