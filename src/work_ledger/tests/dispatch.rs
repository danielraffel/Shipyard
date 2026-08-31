use super::*;
use crate::work_ledger::dispatch::{
    DeliveryFence, FreshAgentLaunchProfile, ProviderAdapter, ProviderCapability,
    ProviderLaunchRequest, ProviderOutcome, WakeConsumerPolicy, WakeDeliveryResult, WakeEnvelope,
    WakeProfileResolver, reconciliation_fence_digest,
};

#[derive(Clone)]
struct TestProfile {
    provider: String,
    launch_options: FreshAgentProviderLaunchOptions,
    digest: String,
    repository: String,
    permits_fresh: bool,
    route_profile_ref: Option<String>,
}

impl FreshAgentLaunchProfile for TestProfile {
    fn provider_id(&self) -> &str {
        &self.provider
    }

    fn provider_launch_options(&self) -> FreshAgentProviderLaunchOptions {
        self.launch_options.clone()
    }

    fn profile_digest(&self) -> WorkLedgerResult<String> {
        Ok(self.digest.clone())
    }

    fn permits_fresh_agent(&self) -> bool {
        self.permits_fresh
    }

    fn protected_profile_bytes(&self) -> WorkLedgerResult<Vec<u8>> {
        Ok(b"profile".to_vec())
    }

    fn resume_expectation(&self) -> Option<FreshAgentResumeExpectation<'_>> {
        Some(FreshAgentResumeExpectation {
            workstream_handle: "GEN-43",
            context_url: Some("https://linear.app/generous/issue/GEN-43"),
            plan_sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            root_revision: 0,
            issue_revision: 0,
            projection_revision: 4,
            material_event_revision: 0,
            checkpoint_id: "wsc_test",
            checkpoint_generation: 1,
            checkpoint_digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            repository: &self.repository,
            head_sha: "0123456789012345678901234567890123456789",
            expected_resume_context_digest: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            success_continuation_digest: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
            failure_continuation_digest: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
        })
    }

    fn route_profile_ref(&self) -> WorkLedgerResult<String> {
        self.route_profile_ref.clone().map_or_else(
            || {
                Ok(OpaqueRef::derive("launch-profile", self.digest.as_bytes())
                    .as_str()
                    .to_owned())
            },
            Ok,
        )
    }
}

struct Resolver {
    profile: TestProfile,
    calls: usize,
}

impl WakeProfileResolver for Resolver {
    type Profile = TestProfile;

    fn resolve(&mut self, _wake: &WakeEnvelope) -> WorkLedgerResult<Self::Profile> {
        self.calls += 1;
        Ok(self.profile.clone())
    }
}

struct RepositoryIsolatingResolver {
    healthy: TestProfile,
    refused_repository: String,
    calls: Vec<String>,
}

impl WakeProfileResolver for RepositoryIsolatingResolver {
    type Profile = TestProfile;

    fn resolve(&mut self, wake: &WakeEnvelope) -> WorkLedgerResult<Self::Profile> {
        self.calls.push(wake.repository.clone());
        if wake.repository == self.refused_repository {
            Err(WorkLedgerError::Refused(
                "malformed repository launch profile".to_owned(),
            ))
        } else {
            Ok(self.healthy.clone())
        }
    }
}

struct Adapter {
    capability: Option<ProviderCapability>,
    launch_outcomes: Vec<ProviderOutcome>,
    reconcile_outcome: ProviderOutcome,
    launch_count: usize,
    launch_fences: Vec<DeliveryFence>,
    reconcile_fences: Vec<DeliveryFence>,
    panic_after_claim: bool,
    panic_during_submit_authorization: bool,
    authorization_refusal: Option<ProviderOutcome>,
    authorization_count: usize,
}

impl Adapter {
    fn successful(idempotent: bool) -> Self {
        Self {
            capability: Some(ProviderCapability {
                adapter_id: "test-provider-adapter".to_owned(),
                fresh_agent_launch: true,
                idempotent_launch: idempotent,
            }),
            launch_outcomes: vec![ProviderOutcome::Delivered {
                receipt: b"launch receipt".to_vec(),
            }],
            reconcile_outcome: ProviderOutcome::Delivered {
                receipt: b"reconciled receipt".to_vec(),
            },
            launch_count: 0,
            launch_fences: Vec::new(),
            reconcile_fences: Vec::new(),
            panic_after_claim: false,
            panic_during_submit_authorization: false,
            authorization_refusal: None,
            authorization_count: 0,
        }
    }
}

impl ProviderAdapter for Adapter {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
        (provider_id == "subrouter")
            .then(|| self.capability.clone())
            .flatten()
    }

    fn authorize(
        &mut self,
        fence: &DeliveryFence,
        operation: ProviderAuthorizationOperation,
    ) -> Result<DeliveryAuthorization, ProviderOutcome> {
        self.authorization_count += 1;
        assert!(
            !(self.panic_during_submit_authorization
                && operation == ProviderAuthorizationOperation::Submit),
            "simulated process death after durable preparation"
        );
        if let Some(refusal) = self.authorization_refusal.take() {
            return Err(refusal);
        }
        Ok(DeliveryAuthorization::for_test(
            fence.work_generation,
            fence.owner_generation,
        ))
    }

    fn authorize_reconciliation(
        &mut self,
        fence: &DeliveryFence,
    ) -> Result<ReconciliationAuthorization, ProviderOutcome> {
        self.authorization_count += 1;
        if let Some(refusal) = self.authorization_refusal.take() {
            return Err(refusal);
        }
        Ok(ReconciliationAuthorization::for_test(
            reconciliation_fence_digest(fence),
        ))
    }

    fn launch(
        &mut self,
        request: ProviderLaunchRequest<'_>,
        _authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        self.launch_count += 1;
        self.launch_fences.push(request.fence.clone());
        assert!(
            !self.panic_after_claim,
            "simulated process death after durable claim"
        );
        self.launch_outcomes.remove(0)
    }

    fn reconcile(
        &mut self,
        fence: &DeliveryFence,
        _authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        self.reconcile_fences.push(fence.clone());
        self.reconcile_outcome.clone()
    }

    fn reconcile_read_only(
        &mut self,
        fence: &DeliveryFence,
        _authority: ReconciliationAuthorization,
    ) -> ProviderOutcome {
        self.reconcile_fences.push(fence.clone());
        self.reconcile_outcome.clone()
    }
}

#[test]
fn authority_refusal_never_marks_launched_or_enters_provider() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.authorization_refusal = Some(ProviderOutcome::Rejected {
        evidence: b"delivery-authority-refused:head_mismatch".to_vec(),
    });

    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("durable authority refusal"),
        WakeDeliveryResult::Failed
    );
    assert_eq!(adapter.authorization_count, 1);
    assert_eq!(adapter.launch_count, 0);
    let from_state: String = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT from_state FROM provider_delivery_observations observation
             JOIN provider_deliveries delivery USING(delivery_id)
             WHERE delivery.wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("authority refusal observation");
    assert_eq!(from_state, "prepared");
}

struct DriftingReconcileAdapter {
    inner: Adapter,
    database_path: std::path::PathBuf,
    work_item_id: String,
}

impl ProviderAdapter for DriftingReconcileAdapter {
    fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
        self.inner.capability(provider_id)
    }

    fn authorize(
        &mut self,
        fence: &DeliveryFence,
        operation: ProviderAuthorizationOperation,
    ) -> Result<DeliveryAuthorization, ProviderOutcome> {
        self.inner.authorize(fence, operation)
    }

    fn authorize_reconciliation(
        &mut self,
        fence: &DeliveryFence,
    ) -> Result<ReconciliationAuthorization, ProviderOutcome> {
        self.inner.authorize_reconciliation(fence)
    }

    fn launch(
        &mut self,
        request: ProviderLaunchRequest<'_>,
        authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        self.inner.launch(request, authority)
    }

    fn reconcile(
        &mut self,
        fence: &DeliveryFence,
        authority: DeliveryAuthorization,
    ) -> ProviderOutcome {
        rusqlite::Connection::open(&self.database_path)
            .expect("concurrent connection")
            .execute(
                "UPDATE work_items SET work_generation = work_generation + 1 WHERE id = ?1",
                [&self.work_item_id],
            )
            .expect("plant concurrent lifecycle change");
        self.inner.reconcile(fence, authority)
    }

    fn reconcile_read_only(
        &mut self,
        fence: &DeliveryFence,
        authority: ReconciliationAuthorization,
    ) -> ProviderOutcome {
        rusqlite::Connection::open(&self.database_path)
            .expect("concurrent connection")
            .execute(
                "UPDATE work_items SET work_generation = work_generation + 1 WHERE id = ?1",
                [&self.work_item_id],
            )
            .expect("plant concurrent lifecycle change");
        self.inner.reconcile_read_only(fence, authority)
    }
}

fn active_policy() -> WakeConsumerPolicy {
    WakeConsumerPolicy {
        activation_enabled: true,
        dispatch_enabled: true,
        authorized_repositories: vec!["danielraffel/pulp".to_owned()],
    }
}

fn setup_wake() -> (tempfile::TempDir, WorkLedger, TestProfile, String, String) {
    let temp = tempfile::TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let (profile, work_id, wake_id) = add_wake(&ledger, sample_candidate());
    (temp, ledger, profile, work_id, wake_id)
}

fn add_wake(ledger: &WorkLedger, candidate: ImportCandidate) -> (TestProfile, String, String) {
    add_wake_labeled(ledger, candidate, "", true)
}

fn add_wake_labeled(
    ledger: &WorkLedger,
    candidate: ImportCandidate,
    route_suffix: &str,
    register_adapter: bool,
) -> (TestProfile, String, String) {
    let work_id = candidate.work_id.clone();
    ledger.import(&[candidate]).expect("import");
    ledger
        .record_continuations(
            &work_id,
            0,
            &ContinuationSet::new(digest(b"success"), None, digest(b"failure"), None)
                .expect("continuations"),
        )
        .expect("record continuations");
    for (generation, state) in [
        (1, LifecycleState::Published),
        (2, LifecycleState::Ready),
        (3, LifecycleState::Managed),
        (4, LifecycleState::Actionable),
    ] {
        ledger
            .transition_with_wake(&work_id, generation, 3, state, None)
            .expect("transition");
    }
    let (route, agent_adapter) = sample_route_labeled(&work_id, 5, route_suffix);
    if register_adapter {
        ledger.register_adapter(&agent_adapter).expect("adapter");
    }
    ledger.register_route(&route).expect("route");
    let profile = TestProfile {
        provider: "subrouter".to_owned(),
        launch_options: FreshAgentProviderLaunchOptions {
            model_id: Some("model-tier-a".to_owned()),
            reasoning_effort: None,
        },
        digest: digest(b"profile"),
        repository: "danielraffel/pulp".to_owned(),
        permits_fresh: true,
        route_profile_ref: None,
    };
    let wake = WakeIntent::new(
        &work_id,
        6,
        3,
        route.route_ref.clone(),
        profile.digest.clone(),
    )
    .expect("wake");
    let wake_id = wake.wake_id.clone();
    ledger
        .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&wake))
        .expect("dispatch transition");
    (profile, work_id, wake_id)
}

fn outbox_state(ledger: &WorkLedger, wake_id: &str) -> String {
    ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [wake_id],
            |row| row.get(0),
        )
        .expect("outbox state")
}

fn context_receipt(ledger: &WorkLedger, wake_id: &str) -> AgentContextReceipt {
    let authority: (String, u64, u64, String, String, String) = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT wake.work_item_id, wake.work_generation, wake.owner_generation,
                    delivery.delivery_id, delivery.idempotency_key, receipt.content_digest
             FROM outbox wake
             JOIN provider_deliveries delivery
               ON delivery.delivery_id = wake.provider_delivery_id
             JOIN protected_objects receipt
               ON receipt.object_ref = delivery.receipt_object_ref
             WHERE wake.wake_id = ?1",
            [wake_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .expect("delivered authority");
    AgentContextReceipt {
        schema_version: 1,
        wake_id: wake_id.to_owned(),
        work_item_id: authority.0,
        work_generation: authority.1,
        owner_generation: authority.2,
        delivery_id: authority.3,
        idempotency_key: authority.4,
        provider_receipt_digest: authority.5,
        workstream_handle: "GEN-43".to_owned(),
        context_url: Some("https://linear.app/generous/issue/GEN-43".to_owned()),
        plan_sha256: "a".repeat(64),
        root_revision: 0,
        issue_revision: 0,
        material_event_revision: 0,
        projection_revision: 4,
        checkpoint_id: "wsc_test".to_owned(),
        checkpoint_generation: 1,
        checkpoint_digest: "b".repeat(64),
        repository: "danielraffel/pulp".to_owned(),
        head_sha: "0123456789012345678901234567890123456789".to_owned(),
        resume_context_digest: "c".repeat(64),
        success_continuation_digest: "d".repeat(64),
        failure_continuation_digest: "e".repeat(64),
    }
}

fn return_expectation(
    work_item_id: &str,
    ownership_id: &str,
    context_receipt_digest: &str,
    delivery_id: &str,
) -> AgentReturnExpectation {
    AgentReturnExpectation {
        schema_version: 1,
        work_item_id: work_item_id.to_owned(),
        ownership_id: ownership_id.to_owned(),
        delivery_id: delivery_id.to_owned(),
        work_generation: 7,
        owner_generation: 3,
        context_receipt_digest: context_receipt_digest.to_owned(),
        checkpoint_id: "wsc_returned".to_owned(),
        checkpoint_generation: 2,
        checkpoint_digest: "1".repeat(64),
        repository: "danielraffel/pulp".to_owned(),
        head_sha: "1234567890123456789012345678901234567890".to_owned(),
        evidence_digest: "2".repeat(64),
        remote_acknowledgement_digest: "3".repeat(64),
    }
}

fn return_receipt(expected: &AgentReturnExpectation) -> AgentReturnReceipt {
    AgentReturnReceipt {
        schema_version: expected.schema_version,
        work_item_id: expected.work_item_id.clone(),
        ownership_id: expected.ownership_id.clone(),
        delivery_id: expected.delivery_id.clone(),
        work_generation: expected.work_generation,
        owner_generation: expected.owner_generation,
        context_receipt_digest: expected.context_receipt_digest.clone(),
        checkpoint_id: expected.checkpoint_id.clone(),
        checkpoint_generation: expected.checkpoint_generation,
        checkpoint_digest: expected.checkpoint_digest.clone(),
        repository: expected.repository.clone(),
        head_sha: expected.head_sha.clone(),
        evidence_digest: expected.evidence_digest.clone(),
        remote_acknowledgement_digest: expected.remote_acknowledgement_digest.clone(),
    }
}

fn deliver_wake(ledger: &WorkLedger, profile: TestProfile) -> (String, String) {
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("deliver"),
        WakeDeliveryResult::Delivered
    );
    let fence = adapter.launch_fences.pop().expect("delivery fence");
    (fence.wake_id, fence.delivery_id)
}

#[test]
fn success_persists_only_typed_launch_options_without_claiming_agent_ownership() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut adapter = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("consume"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(adapter.launch_count, 1);
    assert_eq!(adapter.launch_fences.len(), 1);
    assert_eq!(outbox_state(&ledger, &wake_id), "delivered");
    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work state");
    assert_eq!(work, ("dispatching".to_owned(), 6));
    let attempt: (String, bool) = connection
        .query_row(
            "SELECT state, finished_at IS NOT NULL FROM wake_attempts WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("attempt");
    assert_eq!(attempt, ("delivered".to_owned(), true));
    let delivery: (String, String, usize) = connection
        .query_row(
            "SELECT delivery.state, activation.state, length(delivery.idempotency_key)
             FROM provider_deliveries delivery
             JOIN activation_epochs activation
               ON activation.activation_id = delivery.activation_id
             WHERE delivery.wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("durable delivery");
    assert_eq!(
        delivery,
        ("delivered".to_owned(), "released".to_owned(), 64)
    );
    assert_eq!(ledger.status().expect("status").protected_objects, 3);
    let (_, request_bytes) = ledger
        .open_protected_object(&adapter.launch_fences[0].request_object_ref)
        .expect("stored provider request");
    let stored: serde_json::Value = serde_json::from_slice(&request_bytes).expect("request JSON");
    assert_eq!(stored["schema_version"], 2);
    assert!(stored.get("argv").is_none());
    assert!(stored.get("launch_argv").is_none());
    assert!(stored.get("resume_argv").is_none());
    assert_eq!(stored["launch_options"]["model_id"], "model-tier-a");
    let decoded: StoredProviderRequest =
        serde_json::from_value(stored.clone()).expect("schema-v2 request");
    assert_eq!(decoded.schema_version, 2);
    let mut legacy = stored;
    legacy["schema_version"] = serde_json::json!(1);
    legacy
        .as_object_mut()
        .expect("request object")
        .remove("launch_options");
    legacy["argv"] = serde_json::json!(["provider", "raw prompt secret"]);
    assert!(
        serde_json::from_value::<StoredProviderRequest>(legacy).is_err(),
        "legacy argv-bearing requests must fail closed"
    );
}

#[test]
fn repository_allowlist_skips_unauthorized_wake_without_mutation_or_starvation() {
    let temp = tempfile::TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let mut unauthorized = sample_candidate();
    unauthorized.repo = Some("attacker/private".to_owned());
    let (_unauthorized_profile, _unauthorized_work, unauthorized_wake) =
        add_wake(&ledger, unauthorized);

    let mut allowed = sample_candidate();
    allowed.work_id = opaque_ref("wi", "allowed shipyard continuation");
    allowed.repo = Some("generous-corp/shipyard".to_owned());
    allowed.source_ref = opaque_ref("src", "allowed shipyard continuation");
    allowed.content_digest = digest(b"allowed shipyard continuation");
    let (mut allowed_profile, _allowed_work, allowed_wake) =
        add_wake_labeled(&ledger, allowed, "allowed", false);
    allowed_profile.repository = "generous-corp/shipyard".to_owned();
    let policy = WakeConsumerPolicy {
        activation_enabled: true,
        dispatch_enabled: true,
        authorized_repositories: vec!["generous-corp/shipyard".to_owned()],
    };
    let mut resolver = Resolver {
        profile: allowed_profile,
        calls: 0,
    };
    let mut adapter = Adapter::successful(true);

    assert!(
        ledger
            .has_authorized_pending_wake(&policy)
            .expect("allowed wake is selectable")
    );

    assert_eq!(
        ledger
            .consume_one_wake(policy.clone(), &mut resolver, &mut adapter)
            .expect("consume allowed wake"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(resolver.calls, 1);
    assert_eq!(outbox_state(&ledger, &allowed_wake), "delivered");
    assert_eq!(outbox_state(&ledger, &unauthorized_wake), "pending");
    assert!(
        !ledger
            .has_authorized_pending_wake(&policy)
            .expect("unauthorized-only queue is idle")
    );
}

#[test]
fn malformed_repository_is_contained_without_starving_healthy_repository() {
    let temp = tempfile::TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let mut malformed = sample_candidate();
    malformed.repo = Some("danielraffel/pulp".to_owned());
    let (_malformed_profile, _malformed_work, malformed_wake) = add_wake(&ledger, malformed);

    let mut healthy = sample_candidate();
    healthy.work_id = opaque_ref("wi", "healthy shipyard continuation");
    healthy.repo = Some("generous-corp/shipyard".to_owned());
    healthy.source_ref = opaque_ref("src", "healthy shipyard continuation");
    healthy.content_digest = digest(b"healthy shipyard continuation");
    let (mut healthy_profile, _healthy_work, healthy_wake) =
        add_wake_labeled(&ledger, healthy, "healthy-repository", false);
    healthy_profile.repository = "generous-corp/shipyard".to_owned();
    let policy = WakeConsumerPolicy {
        activation_enabled: true,
        dispatch_enabled: true,
        authorized_repositories: vec![
            "danielraffel/pulp".to_owned(),
            "generous-corp/shipyard".to_owned(),
        ],
    };
    let mut resolver = RepositoryIsolatingResolver {
        healthy: healthy_profile,
        refused_repository: "danielraffel/pulp".to_owned(),
        calls: Vec::new(),
    };
    let mut adapter = Adapter::successful(true);

    assert_eq!(
        ledger
            .consume_one_wake(policy, &mut resolver, &mut adapter)
            .expect("healthy repository progresses around malformed peer"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(
        resolver.calls,
        vec![
            "danielraffel/pulp".to_owned(),
            "generous-corp/shipyard".to_owned(),
        ]
    );
    assert_eq!(outbox_state(&ledger, &malformed_wake), "pending");
    assert_eq!(outbox_state(&ledger, &healthy_wake), "delivered");
    assert_eq!(adapter.launch_count, 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn delivered_context_ack_and_return_are_separate_exact_replayable_cas_steps() {
    let (temp, ledger, profile, work_id, _wake_id) = setup_wake();
    ledger
        .bind_workstream_projection(
            &work_id,
            "GEN-43",
            &"a".repeat(64),
            0,
            0,
            4,
            0,
            "github.com",
            "R_test_pulp",
            "danielraffel/pulp",
            "0123456789012345678901234567890123456789",
        )
        .expect("projection binding");
    let (wake_id, delivery_id) = deliver_wake(&ledger, profile);
    let context = context_receipt(&ledger, &wake_id);
    assert_eq!(
        ledger
            .agent_context_challenge(&wake_id, &["danielraffel/pulp".to_owned()])
            .expect("context challenge"),
        context
    );
    assert!(
        ledger
            .agent_context_challenge(&wake_id, &["attacker/private".to_owned()])
            .is_err()
    );
    let context_bytes = serde_json::to_vec(&context).expect("context receipt");
    let ownership = ledger
        .acknowledge_agent_context(&wake_id, &context_bytes)
        .expect("context acknowledgement");
    assert_eq!(outbox_state(&ledger, &wake_id), "acknowledged");
    let restarted = WorkLedger::open_existing(temp.path())
        .expect("reopen")
        .expect("persisted ledger");
    let replay = restarted
        .acknowledge_agent_context(&wake_id, &context_bytes)
        .expect("ack replay after restart");
    assert_eq!(replay, ownership);

    let challenge = restarted
        .agent_return_challenge(&ownership.ownership_id, &["danielraffel/pulp".to_owned()])
        .expect("return challenge");
    assert_eq!(challenge.work_item_id, work_id);
    assert_eq!(challenge.delivery_id, delivery_id);
    assert_eq!(challenge.work_generation, 7);
    assert_eq!(challenge.checkpoint_generation, 1);

    let expected = return_expectation(
        &work_id,
        &ownership.ownership_id,
        &ownership.receipt_digest,
        &delivery_id,
    );
    let return_bytes = serde_json::to_vec(&return_receipt(&expected)).expect("return receipt");
    let returned = restarted
        .return_agent_ownership(
            &ownership.ownership_id,
            &delivery_id,
            7,
            &expected,
            &return_bytes,
        )
        .expect("ownership return");
    let return_replay = WorkLedger::open_existing(temp.path())
        .expect("reopen returned")
        .expect("persisted returned ledger")
        .return_agent_ownership(
            &ownership.ownership_id,
            &delivery_id,
            7,
            &expected,
            &return_bytes,
        )
        .expect("return replay");
    assert_eq!(return_replay, returned);
    assert_eq!(
        restarted
            .agent_context_challenge(&wake_id, &["danielraffel/pulp".to_owned()])
            .expect("CLI ack preflight after lost returned response"),
        context
    );
    assert_eq!(
        restarted
            .acknowledge_agent_context(&wake_id, &context_bytes)
            .expect("CLI ack replay after ownership returned"),
        ownership
    );
    assert_eq!(
        restarted
            .agent_return_challenge(&ownership.ownership_id, &["danielraffel/pulp".to_owned()],)
            .expect("CLI return preflight after lost returned response")
            .ownership_id,
        ownership.ownership_id
    );
    let mut other_expected = expected.clone();
    other_expected.evidence_digest = "9".repeat(64);
    let other_receipt = serde_json::to_vec(&return_receipt(&other_expected)).expect("other return");
    assert!(
        restarted
            .return_agent_ownership(
                &ownership.ownership_id,
                &delivery_id,
                7,
                &other_expected,
                &other_receipt,
            )
            .is_err(),
        "a different receipt cannot replay an already returned ownership"
    );
    let work: (String, u64, String) = restarted
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT phase, work_generation, head_sha FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("returned work");
    assert_eq!(work, ("returned".to_owned(), 8, expected.head_sha.clone()));
    let projection_head: String = restarted
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT exact_head FROM workstream_projection_bindings WHERE work_item_id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("projection head");
    assert_eq!(projection_head, expected.head_sha);
    assert!(
        restarted
            .connect_read_write()
            .expect("connection")
            .execute(
                "UPDATE workstream_projection_bindings SET exact_head = ?1
                  WHERE work_item_id = ?2",
                rusqlite::params!["f".repeat(40), work_id],
            )
            .is_err(),
        "unfenced projection-head mutation must remain rejected"
    );
    let forged_head = "e".repeat(40);
    let mut connection = restarted.connect_read_write().expect("forgery connection");
    let transaction = connection.transaction().expect("forgery transaction");
    transaction
        .execute(
            "UPDATE work_items SET head_sha = ?1 WHERE id = ?2 AND phase = 'returned'",
            rusqlite::params![forged_head, work_id],
        )
        .expect("mutable work row alone is not projection authority");
    assert!(
        transaction
            .execute(
                "UPDATE workstream_projection_bindings SET exact_head = ?1
                  WHERE work_item_id = ?2",
                rusqlite::params![forged_head, work_id],
            )
            .is_err(),
        "two-step work plus binding SQL forgery must be rejected"
    );
    drop(transaction);
    let unchanged: (String, String) = connection
        .query_row(
            "SELECT work.head_sha, binding.exact_head
               FROM work_items work
               JOIN workstream_projection_bindings binding ON binding.work_item_id = work.id
              WHERE work.id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("rolled-back exact heads");
    assert_eq!(unchanged, (expected.head_sha.clone(), expected.head_sha));
}

#[test]
fn context_and_return_receipts_refuse_drift_without_partial_transition() {
    let (_temp, ledger, profile, work_id, _wake_id) = setup_wake();
    let (wake_id, delivery_id) = deliver_wake(&ledger, profile);
    let mut wrong_context = context_receipt(&ledger, &wake_id);
    wrong_context.checkpoint_digest = "f".repeat(64);
    assert!(
        ledger
            .acknowledge_agent_context(
                &wake_id,
                &serde_json::to_vec(&wrong_context).expect("wrong receipt"),
            )
            .is_err()
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "delivered");
    let context_bytes =
        serde_json::to_vec(&context_receipt(&ledger, &wake_id)).expect("context receipt");
    let ownership = ledger
        .acknowledge_agent_context(&wake_id, &context_bytes)
        .expect("valid acknowledgement");
    let expected = return_expectation(
        &work_id,
        &ownership.ownership_id,
        &ownership.receipt_digest,
        &delivery_id,
    );
    let mut unchanged_head = expected.clone();
    unchanged_head.head_sha = context_receipt(&ledger, &wake_id).head_sha;
    assert!(
        ledger
            .return_agent_ownership(
                &ownership.ownership_id,
                &delivery_id,
                7,
                &unchanged_head,
                &serde_json::to_vec(&return_receipt(&unchanged_head))
                    .expect("unchanged-head return"),
            )
            .is_err(),
        "ownership return must advance to a distinct remotely acknowledged head"
    );
    let mut wrong_return = return_receipt(&expected);
    wrong_return.head_sha = "2234567890123456789012345678901234567890".to_owned();
    assert!(
        ledger
            .return_agent_ownership(
                &ownership.ownership_id,
                &opaque_ref("pd", "wrong delivery"),
                7,
                &expected,
                &serde_json::to_vec(&wrong_return).expect("wrong return"),
            )
            .is_err()
    );
    assert!(
        ledger
            .return_agent_ownership(
                &ownership.ownership_id,
                &delivery_id,
                7,
                &expected,
                &serde_json::to_vec(&wrong_return).expect("wrong head"),
            )
            .is_err()
    );
    wrong_return.head_sha = expected.head_sha.clone();
    wrong_return.checkpoint_generation = 1;
    assert!(
        ledger
            .return_agent_ownership(
                &ownership.ownership_id,
                &delivery_id,
                7,
                &expected,
                &serde_json::to_vec(&wrong_return).expect("stale checkpoint"),
            )
            .is_err()
    );
    wrong_return.checkpoint_generation = expected.checkpoint_generation;
    wrong_return.remote_acknowledgement_digest = "0".repeat(64);
    assert!(
        ledger
            .return_agent_ownership(
                &ownership.ownership_id,
                &delivery_id,
                7,
                &expected,
                &serde_json::to_vec(&wrong_return).expect("unacknowledged checkpoint"),
            )
            .is_err()
    );
}

#[test]
fn context_and_return_receipts_cannot_cross_delivery_or_ownership_authority() {
    let (_temp_a, ledger_a, profile_a, work_a, _wake_a) = setup_wake();
    let temp_b = tempfile::TempDir::new().expect("temp B");
    let ledger_b = WorkLedger::open(temp_b.path()).expect("ledger B");
    let mut candidate_b = sample_candidate();
    candidate_b.work_id = opaque_ref("wi", "cross-authority work B");
    candidate_b.source_ref = opaque_ref("src", "cross-authority work B");
    candidate_b.content_digest = digest(b"cross-authority work B");
    let (profile_b, work_b, _wake_b) = add_wake(&ledger_b, candidate_b);
    let (wake_a, delivery_a) = deliver_wake(&ledger_a, profile_a);
    let (wake_b, delivery_b) = deliver_wake(&ledger_b, profile_b);

    let context_a = context_receipt(&ledger_a, &wake_a);
    let context_b = context_receipt(&ledger_b, &wake_b);
    let context_a_bytes = serde_json::to_vec(&context_a).expect("context A");
    assert!(
        ledger_b
            .acknowledge_agent_context(&wake_b, &context_a_bytes)
            .is_err(),
        "a context acknowledgement must bind its exact delivery"
    );
    let ownership_a = ledger_a
        .acknowledge_agent_context(&wake_a, &context_a_bytes)
        .expect("ownership A");
    let ownership_b = ledger_b
        .acknowledge_agent_context(&wake_b, &serde_json::to_vec(&context_b).expect("context B"))
        .expect("ownership B");

    let expected_a = return_expectation(
        &work_a,
        &ownership_a.ownership_id,
        &ownership_a.receipt_digest,
        &delivery_a,
    );
    let expected_b = return_expectation(
        &work_b,
        &ownership_b.ownership_id,
        &ownership_b.receipt_digest,
        &delivery_b,
    );
    let receipt_a = serde_json::to_vec(&return_receipt(&expected_a)).expect("return A");
    assert!(
        ledger_b
            .return_agent_ownership(
                &ownership_b.ownership_id,
                &delivery_b,
                expected_b.work_generation,
                &expected_b,
                &receipt_a,
            )
            .is_err(),
        "an agent return must bind its exact work, delivery, and ownership"
    );
    let state: String = ledger_b
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT state FROM agent_ownership WHERE ownership_id = ?1",
            [&ownership_b.ownership_id],
            |row| row.get(0),
        )
        .expect("ownership state");
    assert_eq!(state, "acknowledged");
}

#[test]
fn rejected_alternate_receipt_replays_do_not_grow_protected_storage() {
    let (_temp, ledger, profile, work_id, _wake_id) = setup_wake();
    let (wake_id, delivery_id) = deliver_wake(&ledger, profile);
    let context = context_receipt(&ledger, &wake_id);
    let context_bytes = serde_json::to_vec(&context).expect("context receipt");
    let ownership = ledger
        .acknowledge_agent_context(&wake_id, &context_bytes)
        .expect("acknowledge");
    let after_ack = ledger.status().expect("status after ack").protected_objects;
    let alternate_context = serde_json::to_vec_pretty(&context).expect("alternate context bytes");
    assert_ne!(digest(&alternate_context), digest(&context_bytes));
    for _ in 0..3 {
        assert!(
            ledger
                .acknowledge_agent_context(&wake_id, &alternate_context)
                .is_err()
        );
    }
    assert_eq!(
        ledger
            .status()
            .expect("status after refused ack")
            .protected_objects,
        after_ack
    );

    let expected = return_expectation(
        &work_id,
        &ownership.ownership_id,
        &ownership.receipt_digest,
        &delivery_id,
    );
    let return_receipt = return_receipt(&expected);
    let return_bytes = serde_json::to_vec(&return_receipt).expect("return receipt");
    ledger
        .return_agent_ownership(
            &ownership.ownership_id,
            &delivery_id,
            expected.work_generation,
            &expected,
            &return_bytes,
        )
        .expect("return");
    let after_return = ledger
        .status()
        .expect("status after return")
        .protected_objects;
    let alternate_return =
        serde_json::to_vec_pretty(&return_receipt).expect("alternate return bytes");
    assert_ne!(digest(&alternate_return), digest(&return_bytes));
    for _ in 0..3 {
        assert!(
            ledger
                .return_agent_ownership(
                    &ownership.ownership_id,
                    &delivery_id,
                    expected.work_generation,
                    &expected,
                    &alternate_return,
                )
                .is_err()
        );
    }
    assert_eq!(
        ledger
            .status()
            .expect("status after refused return")
            .protected_objects,
        after_return
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn concurrent_differing_receipts_publish_only_the_cas_winner() {
    let (temp, ledger, profile, work_id, _wake_id) = setup_wake();
    let (wake_id, delivery_id) = deliver_wake(&ledger, profile);
    drop(ledger);
    let context = context_receipt(
        &WorkLedger::open_existing(temp.path())
            .expect("open")
            .expect("ledger"),
        &wake_id,
    );
    let context_variants = [
        serde_json::to_vec(&context).expect("compact context"),
        serde_json::to_vec_pretty(&context).expect("pretty context"),
    ];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ack_threads = context_variants.clone().map(|bytes| {
        let state_dir = temp.path().to_owned();
        let wake_id = wake_id.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        std::thread::spawn(move || {
            let ledger = WorkLedger::open_existing(&state_dir)
                .expect("open racing ack")
                .expect("ledger");
            barrier.wait();
            ledger.acknowledge_agent_context(&wake_id, &bytes).ok()
        })
    });
    let ack_results = ack_threads.map(|thread| thread.join().expect("ack thread"));
    assert_eq!(
        ack_results.iter().filter(|result| result.is_some()).count(),
        1
    );
    let ack_winner_index = ack_results
        .iter()
        .position(Option::is_some)
        .expect("ack winner");
    let ownership = ack_results[ack_winner_index]
        .clone()
        .expect("winning ownership");
    let reopened = WorkLedger::open_existing(temp.path())
        .expect("reopen after ack")
        .expect("ledger");
    assert_eq!(reopened.status().expect("ack status").protected_objects, 4);
    assert_eq!(
        reopened
            .open_protected_object(&ownership.receipt_object_ref)
            .expect("ack winner object")
            .1,
        context_variants[ack_winner_index]
    );

    let expected = return_expectation(
        &work_id,
        &ownership.ownership_id,
        &ownership.receipt_digest,
        &delivery_id,
    );
    let return_receipt = return_receipt(&expected);
    let return_variants = [
        serde_json::to_vec(&return_receipt).expect("compact return"),
        serde_json::to_vec_pretty(&return_receipt).expect("pretty return"),
    ];
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let return_threads = return_variants.clone().map(|bytes| {
        let state_dir = temp.path().to_owned();
        let ownership_id = ownership.ownership_id.clone();
        let delivery_id = delivery_id.clone();
        let expected = expected.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        std::thread::spawn(move || {
            let ledger = WorkLedger::open_existing(&state_dir)
                .expect("open racing return")
                .expect("ledger");
            barrier.wait();
            ledger
                .return_agent_ownership(
                    &ownership_id,
                    &delivery_id,
                    expected.work_generation,
                    &expected,
                    &bytes,
                )
                .ok()
        })
    });
    let return_results = return_threads.map(|thread| thread.join().expect("return thread"));
    assert_eq!(
        return_results
            .iter()
            .filter(|result| result.is_some())
            .count(),
        1
    );
    let return_winner_index = return_results
        .iter()
        .position(Option::is_some)
        .expect("return winner");
    let returned = return_results[return_winner_index]
        .clone()
        .expect("winning return");
    let reopened = WorkLedger::open_existing(temp.path())
        .expect("reopen after return")
        .expect("ledger");
    assert_eq!(
        reopened.status().expect("return status").protected_objects,
        5
    );
    assert_eq!(
        reopened
            .open_protected_object(&returned.receipt_object_ref)
            .expect("return winner object")
            .1,
        return_variants[return_winner_index]
    );
    assert_eq!(
        reopened
            .return_agent_ownership(
                &ownership.ownership_id,
                &delivery_id,
                expected.work_generation,
                &expected,
                &return_variants[return_winner_index],
            )
            .expect("exact winner replay"),
        returned
    );
}

#[test]
fn retry_is_durable_and_uses_a_new_attempt_without_changing_generation() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![
        ProviderOutcome::Retryable {
            evidence: b"temporary provider refusal".to_vec(),
        },
        ProviderOutcome::Delivered {
            receipt: b"second attempt receipt".to_vec(),
        },
    ];
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("first"),
        WakeDeliveryResult::Retrying
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
    let generation: u64 = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("generation");
    assert_eq!(generation, 6);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("second"),
        WakeDeliveryResult::Delivered
    );
    let attempts: Vec<(u64, String)> = {
        let connection = ledger.connect_read_only().expect("connection");
        let mut query = connection
            .prepare("SELECT attempt, state FROM wake_attempts ORDER BY attempt")
            .expect("query");
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(
        attempts,
        vec![(1, "retry".to_owned()), (2, "delivered".to_owned())]
    );
}

#[test]
fn retry_ceiling_fails_closed_without_allocating_an_unbounded_attempt() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![
        ProviderOutcome::Retryable {
            evidence: b"first temporary provider refusal".to_vec(),
        },
        ProviderOutcome::Retryable {
            evidence: b"second temporary provider refusal".to_vec(),
        },
        ProviderOutcome::Retryable {
            evidence: b"permanent provider refusal".to_vec(),
        },
        ProviderOutcome::Delivered {
            receipt: b"must never launch a fourth attempt".to_vec(),
        },
    ];

    for expected in [WakeDeliveryResult::Retrying, WakeDeliveryResult::Retrying] {
        assert_eq!(
            ledger
                .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
                .expect("bounded retry"),
            expected
        );
        assert_eq!(outbox_state(&ledger, &wake_id), "pending");
    }
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("exhausted retry"),
        WakeDeliveryResult::Failed
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "failed");
    assert_eq!(adapter.launch_outcomes.len(), 1);

    let connection = ledger.connect_read_only().expect("connection");
    let attempts: Vec<(u64, String)> = {
        let mut query = connection
            .prepare("SELECT attempt, state FROM wake_attempts ORDER BY attempt")
            .expect("query");
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(
        attempts,
        vec![
            (1, "retry".to_owned()),
            (2, "retry".to_owned()),
            (3, "failed".to_owned()),
        ]
    );
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("actionable work");
    assert_eq!(work, ("actionable".to_owned(), 7));
    let provider_receipts: u64 = connection
        .query_row(
            "SELECT count(*) FROM protected_objects
             WHERE work_item_id = ?1 AND kind = 'provider_receipt'",
            [&work_id],
            |row| row.get(0),
        )
        .expect("bounded provider receipts");
    assert_eq!(provider_receipts, 3);
    let terminal_event: String = connection
        .query_row(
            "SELECT kind FROM events
             WHERE work_item_id = ?1 AND work_generation = 7",
            [&work_id],
            |row| row.get(0),
        )
        .expect("retry exhaustion event");
    assert_eq!(terminal_event, "provider_retry_exhausted");
}

#[test]
fn retryable_reconciliation_remains_uncertain_and_never_redispatches() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![
        ProviderOutcome::Retryable {
            evidence: b"first temporary provider refusal".to_vec(),
        },
        ProviderOutcome::Retryable {
            evidence: b"second temporary provider refusal".to_vec(),
        },
        ProviderOutcome::Uncertain {
            evidence: b"final attempt outcome unknown".to_vec(),
        },
    ];
    adapter.reconcile_outcome = ProviderOutcome::Retryable {
        evidence: b"final attempt remained retryable".to_vec(),
    };

    for _ in 0..2 {
        assert_eq!(
            ledger
                .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
                .expect("bounded retry"),
            WakeDeliveryResult::Retrying
        );
    }
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("uncertain final attempt"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(
        ledger
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut adapter)
            .expect("non-definitive reconciliation"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
    assert_eq!(adapter.launch_count, 3);
    assert_eq!(adapter.reconcile_fences.len(), 1);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("uncertain wake is never a submit candidate"),
        WakeDeliveryResult::Empty
    );
    assert_eq!(adapter.launch_count, 3);

    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("dispatching work");
    assert_eq!(work, ("dispatching".to_owned(), 6));
    let attempts: u64 = connection
        .query_row(
            "SELECT count(*) FROM wake_attempts WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("bounded attempt count");
    assert_eq!(attempts, 3);
    let terminal_events: u64 = connection
        .query_row(
            "SELECT count(*) FROM events
             WHERE work_item_id = ?1 AND work_generation = 7",
            [&work_id],
            |row| row.get(0),
        )
        .expect("no false terminal event");
    assert_eq!(terminal_events, 0);
}

#[test]
fn definitive_provider_rejection_returns_work_to_actionable() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![ProviderOutcome::Rejected {
        evidence: b"definitive provider rejection".to_vec(),
    }];
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("rejection"),
        WakeDeliveryResult::Failed
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "failed");
    let work: (String, u64) = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("actionable work");
    assert_eq!(work, ("actionable".to_owned(), 7));
}

#[test]
fn restart_reconciles_idempotent_claim_without_duplicate_launch() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(true);
    interrupted.panic_after_claim = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));
    assert!(death.is_err());
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");

    let mut resolver = Resolver { profile, calls: 0 };
    let mut restarted = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("reconcile"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(restarted.launch_count, 0);
    assert_eq!(restarted.reconcile_fences.len(), 1);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("duplicate tick"),
        WakeDeliveryResult::Empty
    );
}

#[test]
fn terminal_stewardship_cancels_prepared_work_before_provider_submit() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut interrupted = Adapter::successful(true);
    interrupted.panic_during_submit_authorization = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut resolver, &mut interrupted);
    }));
    assert!(death.is_err());
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");

    assert!(
        ledger
            .transition_with_wake(&work_id, 6, 3, LifecycleState::Terminal, None)
            .expect("terminal suppression")
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "failed");
    let states: (String, String, String) = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT attempt.state, delivery.state, activation.state
               FROM wake_attempts attempt
               JOIN provider_deliveries delivery
                 ON delivery.wake_id = attempt.wake_id AND delivery.attempt = attempt.attempt
               JOIN activation_epochs activation
                 ON activation.activation_id = delivery.activation_id
              WHERE attempt.wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("suppressed provider state");
    assert_eq!(
        states,
        (
            "failed".to_owned(),
            "failed".to_owned(),
            "released".to_owned()
        )
    );
    assert_eq!(interrupted.launch_count, 0);
}

#[test]
fn terminal_stewardship_defers_launched_claim_for_reconciliation_only() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(true);
    interrupted.panic_after_claim = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));
    assert!(death.is_err());
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");

    assert!(
        !ledger
            .transition_with_wake(&work_id, 6, 3, LifecycleState::Terminal, None)
            .expect("defer terminal while provider outcome is unknown")
    );
    let phase: String = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "dispatching");

    let mut resolver = Resolver { profile, calls: 0 };
    let mut restarted = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("reconcile launched claim"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(restarted.launch_count, 0);
    assert_eq!(restarted.reconcile_fences.len(), 1);
    assert!(
        ledger
            .transition_with_wake(&work_id, 6, 3, LifecycleState::Terminal, None)
            .expect("terminal after reconciliation")
    );
}

#[test]
fn claimed_without_request_is_restart_completed_and_submitted_without_reconcile() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let profile_ref = profile.route_profile_ref().expect("profile ref");
    let claimed_at = Utc::now().to_rfc3339();
    let original_owner = opaque_ref("consumer", "crashed-before-request-publication");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE outbox SET state = 'claimed', profile_ref = ?2, updated_at = ?3
             WHERE wake_id = ?1 AND state = 'pending'",
            params![wake_id, profile_ref, claimed_at],
        )
        .expect("simulate committed claim");
    connection
        .execute(
            "INSERT INTO wake_attempts
             (wake_id, attempt, state, adapter_id, idempotent, started_at)
             VALUES (?1, 1, 'claimed', 'test-provider-adapter', 0, ?2)",
            params![wake_id, claimed_at],
        )
        .expect("simulate claimed attempt");
    connection
        .execute(
            "INSERT INTO wake_claim_epochs
             (wake_id, attempt, epoch, owner_ref, kind, acquired_at)
             VALUES (?1, 1, 1, ?2, 'claim', ?3)",
            params![wake_id, original_owner, claimed_at],
        )
        .expect("simulate original owner epoch");
    drop(connection);

    let mut resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut recovery = Adapter::successful(false);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut recovery)
            .expect("restart completes request then submits"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(recovery.launch_count, 1);
    assert!(recovery.reconcile_fences.is_empty());
    assert_eq!(recovery.launch_fences[0].attempt, 1);
    assert_eq!(outbox_state(&ledger, &wake_id), "delivered");
    let prepared_requests: u64 = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT count(*) FROM protected_objects
             WHERE work_item_id = ?1 AND kind = 'provider_request'",
            [&work_id],
            |row| row.get(0),
        )
        .expect("restart-completed request");
    assert_eq!(prepared_requests, 1);
    let (attempts, owners): (u64, u64) = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT (SELECT count(*) FROM wake_attempts WHERE wake_id = ?1),
                    (SELECT count(DISTINCT owner_ref) FROM wake_claim_epochs WHERE wake_id = ?1)",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("fresh-owner evidence");
    assert_eq!(attempts, 1);
    assert!(owners >= 2);
}

#[test]
fn prepared_request_is_restart_completed_without_reconciliation_or_new_attempt() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(false);
    interrupted.panic_during_submit_authorization = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));
    assert!(death.is_err());
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");
    let delivery_state: String = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT state FROM provider_deliveries WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("prepared delivery");
    assert_eq!(delivery_state, "prepared");

    let mut resolver = Resolver { profile, calls: 0 };
    let mut recovery = Adapter::successful(false);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut recovery)
            .expect("restart launches prepared request"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(recovery.launch_count, 1);
    assert!(recovery.reconcile_fences.is_empty());
    assert_eq!(recovery.launch_fences[0].attempt, 1);
}

#[test]
fn exact_not_delivered_reconciliation_permits_one_fresh_owner_submit() {
    let (_temp, ledger, profile, _work_id, _wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(true);
    interrupted.panic_after_claim = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));
    assert!(death.is_err());

    let mut resolver = Resolver { profile, calls: 0 };
    let mut recovery = Adapter::successful(true);
    recovery.reconcile_outcome = ProviderOutcome::NotDelivered {
        evidence: b"provider proved original idempotency key absent".to_vec(),
    };
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut recovery)
            .expect("exact not-delivered reconciliation"),
        WakeDeliveryResult::Retrying
    );
    assert_eq!(recovery.launch_count, 0);
    assert_eq!(recovery.reconcile_fences.len(), 1);
    let original_key = recovery.reconcile_fences[0].idempotency_key.clone();

    let mut fresh_owner = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut fresh_owner)
            .expect("fresh owner submits only after definitive proof"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(fresh_owner.launch_count, 1);
    assert!(fresh_owner.reconcile_fences.is_empty());
    assert_eq!(fresh_owner.launch_fences[0].attempt, 2);
    assert_ne!(fresh_owner.launch_fences[0].idempotency_key, original_key);
}

#[test]
fn reconciliation_authorization_failure_never_permits_fresh_submit() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(true);
    interrupted.panic_after_claim = true;
    let death = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));
    assert!(death.is_err());

    let mut resolver = Resolver { profile, calls: 0 };
    let mut recovery = Adapter::successful(true);
    recovery.authorization_refusal = Some(ProviderOutcome::Retryable {
        evidence: b"temporary authorization failure".to_vec(),
    });
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut recovery)
            .expect("recovery authorization failure"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
    assert_eq!(recovery.launch_count, 0);
    assert!(recovery.reconcile_fences.is_empty());

    let mut second_recovery = Adapter::successful(true);
    second_recovery.authorization_refusal = Some(ProviderOutcome::Rejected {
        evidence: b"authorization remains unavailable".to_vec(),
    });
    assert_eq!(
        ledger
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut second_recovery)
            .expect("uncertain authorization failure"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
    assert_eq!(second_recovery.launch_count, 0);
    assert!(second_recovery.reconcile_fences.is_empty());
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut second_recovery)
            .expect("uncertain wake is not a submit candidate"),
        WakeDeliveryResult::Empty
    );
}

#[test]
fn non_idempotent_restart_becomes_uncertain_without_launch_or_reconcile() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut first_resolver = Resolver {
        profile: profile.clone(),
        calls: 0,
    };
    let mut interrupted = Adapter::successful(false);
    interrupted.panic_after_claim = true;
    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = ledger.consume_one_wake(active_policy(), &mut first_resolver, &mut interrupted);
    }));

    let mut resolver = Resolver { profile, calls: 0 };
    let mut restarted = Adapter::successful(false);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut restarted)
            .expect("uncertain"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(restarted.launch_count, 0);
    assert!(restarted.reconcile_fences.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
    assert_eq!(
        ledger
            .next_uncertain_wake_id(&active_policy())
            .expect("select uncertain wake"),
        Some(wake_id.clone())
    );
    let original_idempotency = interrupted
        .launch_fences
        .first()
        .expect("original launch fence")
        .idempotency_key
        .clone();
    let mut wrong_adapter = Adapter::successful(false);
    wrong_adapter
        .capability
        .as_mut()
        .expect("capability")
        .adapter_id = "wrong-adapter".to_owned();
    assert!(
        ledger
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut wrong_adapter)
            .is_err()
    );
    assert!(wrong_adapter.reconcile_fences.is_empty());
    assert_eq!(
        ledger
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut restarted)
            .expect("evidence reconciliation"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(restarted.launch_count, 0);
    assert_eq!(restarted.reconcile_fences.len(), 1);
    assert_eq!(
        restarted.reconcile_fences[0].idempotency_key,
        original_idempotency
    );
    assert_eq!(outbox_state(&ledger, &wake_id), "delivered");
}

#[test]
fn uncertain_reconciliation_rechecks_work_authority_after_provider_call() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut first = Adapter::successful(true);
    first.launch_outcomes = vec![ProviderOutcome::Uncertain {
        evidence: b"initial ambiguity".to_vec(),
    }];
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut first)
            .expect("initial uncertainty"),
        WakeDeliveryResult::Uncertain
    );
    let mut drifting = DriftingReconcileAdapter {
        inner: Adapter::successful(true),
        database_path: ledger.path.clone(),
        work_item_id: work_id,
    };
    assert!(
        ledger
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut drifting)
            .is_err(),
        "stale work authority must refuse provider evidence finalization"
    );
    let states: (String, String, String) = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT wake.state, attempt.state, delivery.state
             FROM outbox wake
             JOIN provider_deliveries delivery ON delivery.wake_id = wake.wake_id
             JOIN wake_attempts attempt
               ON attempt.wake_id = delivery.wake_id AND attempt.attempt = delivery.attempt
             WHERE wake.wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("states");
    assert_eq!(
        states,
        (
            "uncertain".to_owned(),
            "uncertain".to_owned(),
            "uncertain".to_owned(),
        )
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn provider_observation_history_is_append_only_ordered_and_survives_reopen() {
    let (temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![ProviderOutcome::Uncertain {
        evidence: b"observation A".to_vec(),
    }];
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("observation A"),
        WakeDeliveryResult::Uncertain
    );
    let original_key = adapter.launch_fences[0].idempotency_key.clone();
    adapter.reconcile_outcome = ProviderOutcome::Uncertain {
        evidence: b"observation B".to_vec(),
    };
    assert_eq!(
        ledger
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut adapter)
            .expect("observation B"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(adapter.launch_count, 1);
    assert_eq!(adapter.reconcile_fences[0].idempotency_key, original_key);
    let attempt_count: u64 = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT count(*) FROM wake_attempts WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("attempt count");
    assert_eq!(attempt_count, 1);
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");

    let reopened = WorkLedger::open(temp.path()).expect("reopen after uncertainty");
    adapter.reconcile_outcome = ProviderOutcome::Delivered {
        receipt: b"observation C".to_vec(),
    };
    assert_eq!(
        reopened
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut adapter)
            .expect("observation C"),
        WakeDeliveryResult::Delivered
    );
    assert_eq!(adapter.launch_count, 1);
    assert_eq!(adapter.reconcile_fences[1].idempotency_key, original_key);
    let reopened = WorkLedger::open(temp.path()).expect("reopen after delivery");
    let rows: Vec<(u64, String, String, String, String)> = {
        let connection = reopened.connect_read_only().expect("connection");
        let mut statement = connection
            .prepare(
                "SELECT observation.sequence, observation.from_state, observation.to_state,
                        observation.outcome_digest, receipt.content_digest
                   FROM provider_delivery_observations observation
                   JOIN protected_objects receipt
                     ON receipt.object_ref = observation.receipt_object_ref
                  ORDER BY observation.sequence",
            )
            .expect("observations");
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(
        rows,
        vec![
            (
                1,
                "launched".to_owned(),
                "uncertain".to_owned(),
                digest(b"observation A"),
                digest(b"observation A"),
            ),
            (
                2,
                "uncertain".to_owned(),
                "uncertain".to_owned(),
                digest(b"observation B"),
                digest(b"observation B"),
            ),
            (
                3,
                "uncertain".to_owned(),
                "delivered".to_owned(),
                digest(b"observation C"),
                digest(b"observation C"),
            ),
        ]
    );
    assert_eq!(reopened.status().expect("status").integrity, "ok");
}

#[test]
fn pre_v3_claim_without_attempt_proof_becomes_uncertain() {
    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    ledger
        .connect_read_write()
        .expect("connection")
        .execute(
            "UPDATE outbox SET state = 'claimed' WHERE wake_id = ?1",
            [&wake_id],
        )
        .expect("simulate migrated v2 claim");
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("conservative recovery"),
        WakeDeliveryResult::Uncertain
    );
    assert_eq!(adapter.launch_count, 0);
    assert!(adapter.reconcile_fences.is_empty());
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");
}

#[test]
fn permanent_uncertainty_quiesces_durably_across_reopen_without_more_receipts() {
    let (temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.launch_outcomes = vec![ProviderOutcome::Uncertain {
        evidence: b"persistent uncertainty 1".to_vec(),
    }];
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("initial uncertainty"),
        WakeDeliveryResult::Uncertain
    );
    for evidence in [
        b"persistent uncertainty 2".as_slice(),
        b"persistent uncertainty 3".as_slice(),
    ] {
        adapter.reconcile_outcome = ProviderOutcome::Uncertain {
            evidence: evidence.to_vec(),
        };
        assert_eq!(
            ledger
                .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut adapter)
                .expect("bounded uncertainty observation"),
            WakeDeliveryResult::Uncertain
        );
    }
    assert_eq!(adapter.launch_count, 1);
    assert_eq!(adapter.reconcile_fences.len(), 2);
    assert_eq!(outbox_state(&ledger, &wake_id), "uncertain");

    let provider_receipts_before: u64 = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT count(*) FROM protected_objects
             WHERE work_item_id = ?1 AND kind = 'provider_receipt'",
            [&work_id],
            |row| row.get(0),
        )
        .expect("provider receipt count");
    assert_eq!(provider_receipts_before, 3);

    for _ in 0..5 {
        let reopened = WorkLedger::open(temp.path()).expect("reopen uncertain ledger");
        assert_eq!(
            reopened
                .next_uncertain_wake_id(&active_policy())
                .expect("quiescent selection"),
            None
        );
        let mut restarted = Adapter::successful(true);
        restarted.reconcile_outcome = ProviderOutcome::Uncertain {
            evidence: b"must not be persisted".to_vec(),
        };
        let refusal = reopened
            .reconcile_uncertain_wake(&active_policy(), &wake_id, &mut restarted)
            .expect_err("automatic reconciliation must quiesce");
        assert!(
            refusal.to_string().contains(
                "uncertain delivery reconciliation budget exhausted; manual intervention required"
            ),
            "unexpected refusal: {refusal}"
        );
        assert!(restarted.reconcile_fences.is_empty());
        let provider_receipts_after: u64 = reopened
            .connect_read_only()
            .expect("connection")
            .query_row(
                "SELECT count(*) FROM protected_objects
                 WHERE work_item_id = ?1 AND kind = 'provider_receipt'",
                [&work_id],
                |row| row.get(0),
            )
            .expect("stable provider receipt count");
        assert_eq!(provider_receipts_after, provider_receipts_before);
        assert_eq!(outbox_state(&reopened, &wake_id), "uncertain");
    }
}

#[test]
fn stale_generation_refuses_before_provider_launch() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    ledger
        .connect_read_write()
        .expect("connection")
        .execute(
            "UPDATE work_items SET work_generation = work_generation + 1 WHERE id = ?1",
            [&work_id],
        )
        .expect("simulate newer owner work");
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
        .expect_err("stale generation");
    assert!(error.to_string().contains("generation is stale"));
    assert_eq!(adapter.launch_count, 0);
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
}

#[test]
fn missing_provider_capability_fails_durably_without_launch() {
    let (_temp, ledger, profile, work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    adapter.capability = None;
    assert_eq!(
        ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect("missing capability"),
        WakeDeliveryResult::Failed
    );
    assert_eq!(adapter.launch_count, 0);
    assert_eq!(outbox_state(&ledger, &wake_id), "failed");
    let phase: String = ledger
        .connect_read_only()
        .expect("connection")
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("work phase");
    assert_eq!(phase, "actionable");
}

#[test]
fn route_profile_identity_mismatch_refuses_before_claim_or_launch() {
    let (_temp, ledger, mut profile, _work_id, wake_id) = setup_wake();
    profile.route_profile_ref = Some(
        OpaqueRef::derive("launch-profile", b"different protected profile")
            .as_str()
            .to_owned(),
    );
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
        .expect_err("route/profile mismatch");
    assert!(error.to_string().contains("wake route is missing, stale"));
    assert_eq!(adapter.launch_count, 0);
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
}

#[test]
fn route_provider_identity_mismatch_refuses_before_claim_or_launch() {
    let (_temp, ledger, mut profile, _work_id, wake_id) = setup_wake();
    profile.provider = "direct".to_owned();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
        .expect_err("route/provider mismatch");
    assert!(error.to_string().contains("wake route is missing, stale"));
    assert_eq!(adapter.launch_count, 0);
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
}

#[test]
#[allow(clippy::too_many_lines)]
fn live_consumer_lease_fences_second_and_third_consumers_during_provider_call() {
    use std::sync::mpsc;

    struct BlockingAdapter {
        entered: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    impl ProviderAdapter for BlockingAdapter {
        fn capability(&self, provider_id: &str) -> Option<ProviderCapability> {
            (provider_id == "subrouter").then(|| ProviderCapability {
                adapter_id: "test-provider-adapter".to_owned(),
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
                reconciliation_fence_digest(fence),
            ))
        }

        fn launch(
            &mut self,
            _request: ProviderLaunchRequest<'_>,
            _authority: DeliveryAuthorization,
        ) -> ProviderOutcome {
            self.entered.send(()).expect("announce provider entry");
            self.release.recv().expect("release provider");
            ProviderOutcome::Delivered {
                receipt: b"barrier launch receipt".to_vec(),
            }
        }

        fn reconcile(
            &mut self,
            _fence: &DeliveryFence,
            _authority: DeliveryAuthorization,
        ) -> ProviderOutcome {
            panic!("a concurrent live owner must not be treated as restart recovery");
        }

        fn reconcile_read_only(
            &mut self,
            _fence: &DeliveryFence,
            _authority: ReconciliationAuthorization,
        ) -> ProviderOutcome {
            panic!("a concurrent live owner must not be treated as restart recovery");
        }
    }

    let (_temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let (entered_tx, entered_rx) = mpsc::sync_channel(0);
    let (release_tx, release_rx) = mpsc::sync_channel(0);
    let first_ledger = ledger.clone();
    let first_profile = profile.clone();
    let first = std::thread::spawn(move || {
        let mut resolver = Resolver {
            profile: first_profile,
            calls: 0,
        };
        let mut adapter = BlockingAdapter {
            entered: entered_tx,
            release: release_rx,
        };
        first_ledger.consume_one_wake(active_policy(), &mut resolver, &mut adapter)
    });
    entered_rx.recv().expect("provider entered");

    for _ in 0..2 {
        let mut resolver = Resolver {
            profile: profile.clone(),
            calls: 0,
        };
        let mut adapter = Adapter::successful(true);
        let error = ledger
            .consume_one_wake(active_policy(), &mut resolver, &mut adapter)
            .expect_err("live consumer owns wake");
        assert!(error.to_string().contains("another live wake consumer"));
        assert_eq!(resolver.calls, 0);
        assert_eq!(adapter.launch_count, 0);
    }
    assert_eq!(outbox_state(&ledger, &wake_id), "claimed");
    release_tx.send(()).expect("release launch");
    assert_eq!(
        first.join().expect("consumer thread").expect("consume"),
        WakeDeliveryResult::Delivered
    );

    let claims: Vec<(u64, String)> = {
        let connection = ledger.connect_read_only().expect("connection");
        let mut query = connection
            .prepare("SELECT epoch, kind FROM wake_claim_epochs ORDER BY epoch")
            .expect("query");
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("rows")
            .collect::<Result<_, _>>()
            .expect("collect")
    };
    assert_eq!(claims, vec![(1, "claim".to_owned())]);
}

#[test]
fn default_off_policy_refuses_before_profile_lookup_or_mutation() {
    let (temp, ledger, profile, _work_id, wake_id) = setup_wake();
    let mut resolver = Resolver { profile, calls: 0 };
    let mut adapter = Adapter::successful(true);
    let error = ledger
        .consume_one_wake(WakeConsumerPolicy::default(), &mut resolver, &mut adapter)
        .expect_err("activation remains off");
    assert!(error.to_string().contains("explicitly enabled"));
    assert_eq!(resolver.calls, 0);
    assert_eq!(adapter.launch_count, 0);
    assert_eq!(outbox_state(&ledger, &wake_id), "pending");
    assert!(!temp.path().join("wake-consumer.lock").exists());
}
