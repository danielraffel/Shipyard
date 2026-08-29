use super::*;
use chrono::{Duration as ChronoDuration, TimeZone};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use crate::provider_wrapper::{
    FreshResumeExpectationV1, ProtectedProviderResponseV1, ProviderAcceptanceV1,
    ProviderLaunchOptionsV1, ProviderWrapperConfig, ProviderWrapperOperationV1,
    ProviderWrapperOutcomeV1, ProviderWrapperResponseV1,
};
use crate::work_ledger::delivery_ownership::{AgentContextReceipt, AgentReturnReceipt};

fn at(second: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2030, 8, 28, 12, 0, second)
        .single()
        .expect("timestamp")
}

fn regressed_wall() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2029, 8, 28, 12, 0, 0)
        .single()
        .expect("regressed timestamp")
}

fn set_time(ledger: &WorkLedger, now: chrono::DateTime<Utc>) {
    ledger.set_manual_time(now).expect("manual ledger clock");
}

fn lease_between(claimed_at: chrono::DateTime<Utc>, expires_at: chrono::DateTime<Utc>) -> Duration {
    expires_at
        .signed_duration_since(claimed_at)
        .to_std()
        .unwrap_or(Duration::ZERO)
}

fn claim_at(
    ledger: &WorkLedger,
    wake_id: &str,
    claimant_ref: &str,
    claimed_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
) -> WorkLedgerResult<DeliveryClaim> {
    set_time(ledger, claimed_at);
    let dispatcher = ledger.dispatcher_epoch()?;
    ledger.claim_wake_in_epoch(
        &dispatcher,
        wake_id,
        claimant_ref,
        lease_between(claimed_at, expires_at),
    )
}

fn claim_in_epoch_at(
    ledger: &WorkLedger,
    dispatcher: &DispatcherEpoch,
    wake_id: &str,
    claimant_ref: &str,
    claimed_at: chrono::DateTime<Utc>,
    expires_at: chrono::DateTime<Utc>,
) -> WorkLedgerResult<DeliveryClaim> {
    set_time(ledger, claimed_at);
    ledger.claim_wake_in_epoch(
        dispatcher,
        wake_id,
        claimant_ref,
        lease_between(claimed_at, expires_at),
    )
}

fn start_at(
    ledger: &WorkLedger,
    claim: &DeliveryClaim,
    started_at: chrono::DateTime<Utc>,
) -> WorkLedgerResult<StartedDelivery> {
    set_time(ledger, started_at);
    ledger.mark_delivery_started(claim)
}

fn reconcile_expired_at(
    ledger: &WorkLedger,
    wake_id: &str,
    observed_at: chrono::DateTime<Utc>,
    receipt_digest: &str,
) -> WorkLedgerResult<ExpiredClaimDisposition> {
    set_time(ledger, observed_at);
    ledger.reconcile_expired_claim(wake_id, receipt_digest)
}

fn acknowledge_at(
    ledger: &WorkLedger,
    started: &StartedDelivery,
    receipt: &DeliveryReceipt,
    acknowledged_at: chrono::DateTime<Utc>,
) -> WorkLedgerResult<()> {
    set_time(ledger, acknowledged_at);
    ledger.acknowledge_delivery(started, receipt)
}

fn reconcile_uncertain_at(
    ledger: &WorkLedger,
    claim: &DeliveryClaim,
    receipt: &DeliveryReceipt,
    reconciled_at: chrono::DateTime<Utc>,
) -> WorkLedgerResult<()> {
    set_time(ledger, reconciled_at);
    ledger.reconcile_uncertain_delivery(claim, receipt)
}

fn fail_unstarted_at(
    ledger: &WorkLedger,
    claim: &DeliveryClaim,
    receipt_digest: &str,
    failed_at: chrono::DateTime<Utc>,
) -> WorkLedgerResult<()> {
    set_time(ledger, failed_at);
    ledger.fail_unstarted_claim(claim, receipt_digest)
}

fn pending_delivery() -> (TempDir, WorkLedger, String, String, AdapterBindingRecord) {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("ledger");
    let candidate = sample_candidate();
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
            .expect("legal transition");
    }
    let (route, adapter) = sample_route(&work_id, 5);
    ledger.register_adapter(&adapter).expect("adapter");
    ledger.register_route(&route).expect("route");
    let wake = ledger
        .wake_intent(&work_id, 6, 3, route.route_ref.clone(), digest(b"payload"))
        .expect("wake");
    let wake_id = wake.wake_id.clone();
    ledger
        .transition_with_wake(&work_id, 5, 3, LifecycleState::Dispatching, Some(&wake))
        .expect("dispatching");
    set_time(&ledger, at(0));
    (temp, ledger, work_id, wake_id, adapter)
}

fn provider_config(claim: &DeliveryClaim) -> ProviderWrapperConfig {
    ProviderWrapperConfig {
        executable_path: std::path::PathBuf::from("/usr/bin/false"),
        executable_sha256: claim.route.executable_sha256.clone(),
        provider_id: claim.route.agent_kind.clone(),
        adapter_id: "subrouter".to_owned(),
        deadline_seconds: 30,
        max_stdout_bytes: 64 * 1024,
        max_stderr_bytes: 64 * 1024,
    }
}

fn resume_expectation(claim: &DeliveryClaim) -> FreshResumeExpectationV1 {
    FreshResumeExpectationV1 {
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
        head_sha: claim.head_sha.clone(),
        expected_resume_context_digest: digest(b"resume"),
        success_continuation_digest: digest(b"success"),
        failure_continuation_digest: digest(b"failure"),
    }
}

fn head_authority(
    claim: &DeliveryClaim,
) -> super::super::provider_publication::RepositoryHeadAuthorityV1 {
    super::super::provider_publication::RepositoryHeadAuthorityV1 {
        repository_id: "R_kgDOR9hrGw".to_owned(),
        installation_ref: OpaqueRef::derive("github-app-installation", b"shipyard")
            .as_str()
            .to_owned(),
        repository: "danielraffel/shipyard".to_owned(),
        head_sha: claim.head_sha.clone(),
        observed_at: claim.claimed_at,
        receipt_digest: digest(b"authenticated exact head"),
    }
}

fn provider_response(
    request: &super::super::provider_publication::PublishedProviderRequest,
) -> ProtectedProviderResponseV1 {
    let wrapper = &request.request.wrapper;
    let response = ProviderWrapperResponseV1 {
        schema_version: 1,
        operation: wrapper.operation,
        provider_id: wrapper.provider_id.clone(),
        adapter_id: wrapper.adapter_id.clone(),
        idempotency_key: wrapper.delivery_fence.idempotency_key.clone(),
        outcome: ProviderWrapperOutcomeV1::Delivered {
            acceptance: ProviderAcceptanceV1::ProviderSessionAccepted,
            provider_session_ref: "session:codex:accepted-1".to_owned(),
            receipt_digest: digest(b"provider receipt"),
        },
    };
    let canonical_bytes = serde_json::to_vec(&response).expect("response bytes");
    ProtectedProviderResponseV1 {
        response_digest: digest(&canonical_bytes),
        canonical_bytes,
    }
}

fn publish_context_request(
    ledger: &WorkLedger,
    claim: &DeliveryClaim,
) -> super::super::provider_publication::PublishedProviderRequest {
    ledger
        .publish_native_provider_request(
            claim,
            ProviderWrapperOperationV1::Submit,
            &provider_config(claim),
            head_authority(claim),
            resume_expectation(claim),
            ProviderLaunchOptionsV1::default(),
        )
        .expect("protected context request")
}

fn add_historical_provider_request(
    ledger: &WorkLedger,
    claim: &DeliveryClaim,
    published: &super::super::provider_publication::PublishedProviderRequest,
) {
    let mut historical = published.request.clone();
    historical.wrapper.delivery_fence.claim_id = opaque_ref("claim", "historical");
    historical.wrapper.delivery_fence.bind_idempotency_key();
    let bytes = serde_json::to_vec(&historical).expect("historical request");
    ledger
        .put_protected_object(
            &claim.work_id,
            ProtectedObjectKind::ProviderRequest,
            None,
            &digest(&bytes),
            &bytes,
        )
        .expect("historical provider request");
}

#[test]
fn provider_request_is_crash_replay_safe_and_preserves_route_axes() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let config = provider_config(&claim);
    let request = ledger
        .publish_native_provider_request(
            &claim,
            ProviderWrapperOperationV1::Submit,
            &config,
            head_authority(&claim),
            resume_expectation(&claim),
            ProviderLaunchOptionsV1::default(),
        )
        .expect("publish request");

    let reopened = WorkLedger::open(temp.path()).expect("reopen after simulated crash");
    let replay = reopened
        .publish_native_provider_request(
            &claim,
            ProviderWrapperOperationV1::Submit,
            &config,
            head_authority(&claim),
            resume_expectation(&claim),
            ProviderLaunchOptionsV1::default(),
        )
        .expect("replay request");

    assert_eq!(request.object, replay.object);
    assert_eq!(request.digest, replay.digest);
    assert_eq!(request.canonical_bytes, replay.canonical_bytes);
    assert!(matches!(
        request.request.route.terminal,
        TerminalRoute::Cmux { .. }
    ));
    assert!(matches!(
        request.request.route.provider,
        ProviderRoute::Subrouter { .. }
    ));
    assert_eq!(request.request.route.account_ref, claim.route.account_ref);
    assert_eq!(request.request.route.model_ref, claim.route.model_ref);
    assert_eq!(
        request.request.route.session_headers_sha256,
        claim.route.session_headers_sha256
    );
    assert_eq!(
        request.request.route.route_revision,
        claim.route.route_revision
    );
}

#[test]
fn provider_request_refuses_stale_claim_and_direct_fallback() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let mut stale = claim.clone();
    stale.work_generation += 1;
    assert!(
        ledger
            .publish_native_provider_request(
                &stale,
                ProviderWrapperOperationV1::Submit,
                &provider_config(&claim),
                head_authority(&claim),
                resume_expectation(&claim),
                ProviderLaunchOptionsV1::default(),
            )
            .is_err()
    );

    let mut stale_authority = head_authority(&claim);
    stale_authority.observed_at = claim.claimed_at - ChronoDuration::seconds(301);
    assert!(
        ledger
            .publish_native_provider_request(
                &claim,
                ProviderWrapperOperationV1::Submit,
                &provider_config(&claim),
                stale_authority,
                resume_expectation(&claim),
                ProviderLaunchOptionsV1::default(),
            )
            .is_err()
    );

    let mut direct = provider_config(&claim);
    direct.adapter_id = "direct".to_owned();
    assert!(
        ledger
            .publish_native_provider_request(
                &claim,
                ProviderWrapperOperationV1::Submit,
                &direct,
                head_authority(&claim),
                resume_expectation(&claim),
                ProviderLaunchOptionsV1::default(),
            )
            .is_err()
    );
}

#[test]
fn provider_receipt_is_exact_and_replay_safe_before_acknowledgment() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let request = ledger
        .publish_native_provider_request(
            &claim,
            ProviderWrapperOperationV1::Submit,
            &provider_config(&claim),
            head_authority(&claim),
            resume_expectation(&claim),
            ProviderLaunchOptionsV1::default(),
        )
        .expect("request");
    let started = start_at(&ledger, &claim, at(1)).expect("start");
    let response = provider_response(&request);
    let receipt = ledger
        .publish_native_provider_receipt(&started, &request, &response)
        .expect("receipt");
    let replay = ledger
        .publish_native_provider_receipt(&started, &request, &response)
        .expect("receipt replay");

    assert_eq!(receipt.object, replay.object);
    assert_ne!(receipt.digest, response.response_digest);
    let (_record, receipt_bytes) = ledger
        .open_protected_object(&receipt.object.object_ref)
        .expect("open protected receipt");
    let attested: super::super::provider_publication::NativeProviderReceiptV1 =
        serde_json::from_slice(&receipt_bytes).expect("decode attested receipt");
    assert_eq!(attested.request_digest, request.digest);
    assert_eq!(attested.route_integrity, claim.route.route_integrity);
    assert_eq!(attested.routing_generation, claim.route.route_revision);
    assert_eq!(
        attested.agent_adapter_generation,
        claim.route.agent.adapter.generation
    );

    let mut mismatched = response.clone();
    let mut decoded: ProviderWrapperResponseV1 =
        serde_json::from_slice(&mismatched.canonical_bytes).expect("decode");
    decoded.idempotency_key = digest(b"wrong fence");
    mismatched.canonical_bytes = serde_json::to_vec(&decoded).expect("re-encode");
    mismatched.response_digest = digest(&mismatched.canonical_bytes);
    assert!(
        ledger
            .publish_native_provider_receipt(&started, &request, &mismatched)
            .is_err()
    );

    let mut cross_delivery = request.clone();
    cross_delivery.request.wrapper.delivery_fence.work_item_id = "work-other".to_owned();
    cross_delivery
        .request
        .wrapper
        .delivery_fence
        .bind_idempotency_key();
    cross_delivery.canonical_bytes =
        serde_json::to_vec(&cross_delivery.request).expect("cross-delivery request bytes");
    cross_delivery.digest = digest(&cross_delivery.canonical_bytes);
    assert!(
        ledger
            .publish_native_provider_receipt(&started, &cross_delivery, &response)
            .is_err()
    );
}

#[test]
fn context_acknowledgement_and_return_are_receipt_fenced() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let published_request = publish_context_request(&ledger, &claim);
    add_historical_provider_request(&ledger, &claim, &published_request);
    let started = start_at(&ledger, &claim, at(1)).expect("start");
    let delivery_receipt = DeliveryReceipt::new(
        &started,
        claim.route.native_session_ref.clone(),
        digest(b"transport"),
    )
    .expect("delivery receipt");
    acknowledge_at(&ledger, &started, &delivery_receipt, at(2)).expect("ack delivery");

    let challenge = ledger
        .agent_context_challenge(&wake_id)
        .expect("context challenge");
    let context = AgentContextReceipt {
        schema_version: 1,
        ownership_id: challenge.ownership_id.clone(),
        wake_id: challenge.wake_id.clone(),
        claim_id: challenge.claim_id.clone(),
        head_sha: challenge.head_sha.clone(),
        delivery_identity_digest: challenge.delivery_identity_digest.clone(),
        reconstructed_context_digest: digest(b"resume"),
        agent_evidence_digest: digest(b"agent evidence"),
    };
    let ownership = ledger
        .acknowledge_agent_context(&challenge, &context)
        .expect("ack context");
    let returned = AgentReturnReceipt {
        schema_version: 1,
        ownership_id: ownership.challenge.ownership_id.clone(),
        context_receipt_digest: ownership.context_receipt_digest.clone(),
        next_checkpoint_digest: digest(b"new checkpoint"),
        evidence_digest: digest(b"completion evidence"),
        remote_acknowledgement_digest: digest(b"remote ack"),
    };
    let returned_digest = digest(&serde_json::to_vec(&returned).expect("return receipt bytes"));
    ledger
        .connect_read_write()
        .expect("writer")
        .execute_batch(
            "CREATE TRIGGER fail_agent_return_event
             BEFORE INSERT ON events
             WHEN NEW.kind = 'agent_ownership_returned'
             BEGIN SELECT RAISE(ABORT, 'simulated event failure'); END;",
        )
        .expect("failure trigger");
    assert!(
        ledger
            .return_agent_ownership(&ownership, &returned)
            .is_err()
    );
    let connection = ledger.connect_read_only().expect("ledger");
    let rolled_back: (String, i64) = connection
        .query_row(
            "SELECT phase,
                    (SELECT COUNT(*) FROM protected_objects WHERE content_digest = ?2)
             FROM work_items WHERE id = ?1",
            rusqlite::params![work_id, returned_digest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("rolled-back return state");
    assert_eq!(rolled_back, ("agent_owned_repair".to_owned(), 0));
    drop(connection);
    ledger
        .connect_read_write()
        .expect("writer")
        .execute_batch("DROP TRIGGER fail_agent_return_event;")
        .expect("drop failure trigger");
    ledger
        .return_agent_ownership(&ownership, &returned)
        .expect("return ownership");

    let phase: String = ledger
        .connect_read_only()
        .expect("ledger")
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "returned");
    assert!(
        ledger
            .return_agent_ownership(&ownership, &returned)
            .is_err()
    );
    assert!(
        ledger
            .acknowledge_agent_context(&challenge, &context)
            .is_err()
    );
}

#[test]
fn context_receipt_mismatch_never_grants_ownership() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    publish_context_request(&ledger, &claim);
    let started = start_at(&ledger, &claim, at(1)).expect("start");
    let receipt = DeliveryReceipt::new(
        &started,
        claim.route.native_session_ref.clone(),
        digest(b"transport"),
    )
    .expect("receipt");
    acknowledge_at(&ledger, &started, &receipt, at(2)).expect("ack");
    let challenge = ledger.agent_context_challenge(&wake_id).expect("challenge");
    let mismatch = AgentContextReceipt {
        schema_version: 1,
        ownership_id: challenge.ownership_id.clone(),
        wake_id: challenge.wake_id.clone(),
        claim_id: challenge.claim_id.clone(),
        head_sha: "ffffffffffffffffffffffffffffffffffffffff".to_owned(),
        delivery_identity_digest: challenge.delivery_identity_digest.clone(),
        reconstructed_context_digest: digest(b"context"),
        agent_evidence_digest: digest(b"evidence"),
    };
    assert!(
        ledger
            .acknowledge_agent_context(&challenge, &mismatch)
            .is_err()
    );
}

#[test]
fn exact_route_generation_precedes_wake_and_claim_by_one() {
    let (_temp, ledger, _work_id, wake_id, adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim route N for work and wake N+1");

    assert_eq!(claim.work_generation, 6);
    assert_eq!(claim.route.terminal_kind, "cmux");
    assert_eq!(claim.route.agent_kind, "codex");
    assert_eq!(claim.route.provider_kind, "subrouter");
    assert!(matches!(claim.route.terminal, TerminalRoute::Cmux { .. }));
    assert!(matches!(claim.route.agent.route, AgentRoute::Codex { .. }));
    assert_eq!(claim.route.agent.adapter, adapter);
    assert!(matches!(
        claim.route.provider,
        ProviderRoute::Subrouter { .. }
    ));
    assert_eq!(claim.route.launch_generation, claim.owner_generation);
    assert!(claim.route.native_resume_ref.starts_with("opaque:sha256:"));
    assert!(claim.route.account_ref.starts_with("opaque:sha256:"));
    assert!(claim.route.model_ref.starts_with("opaque:sha256:"));
}

#[test]
fn generic_agent_ownership_bypass_is_refused() {
    let (_temp, ledger, work_id, _wake_id, _adapter) = pending_delivery();

    let error = ledger
        .transition_with_wake(&work_id, 6, 3, LifecycleState::AgentOwnedRepair, None)
        .expect_err("typed accepted receipt is required");

    assert!(matches!(error, WorkLedgerError::Refused(_)));
    let connection = ledger.connect_read_only().expect("connection");
    assert_eq!(
        connection
            .query_row(
                "SELECT phase FROM work_items WHERE id = ?1",
                [&work_id],
                |row| row.get::<_, String>(0),
            )
            .expect("phase"),
        "dispatching"
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending wake"),
        1
    );
}

#[test]
fn generic_terminal_transition_cannot_strand_an_active_wake() {
    let (_temp, ledger, work_id, _wake_id, _adapter) = pending_delivery();

    let error = ledger
        .transition_with_wake(&work_id, 6, 3, LifecycleState::Terminal, None)
        .expect_err("active delivery requires a typed outcome");

    assert!(matches!(error, WorkLedgerError::Refused(_)));
    let connection = ledger.connect_read_only().expect("connection");
    assert_eq!(
        connection
            .query_row(
                "SELECT phase FROM work_items WHERE id = ?1",
                [&work_id],
                |row| row.get::<_, String>(0),
            )
            .expect("phase"),
        "dispatching"
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending wake"),
        1
    );
}

#[test]
fn concurrent_claim_is_singleton() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for machine in ["m1", "m5"] {
        let ledger = WorkLedger::open(temp.path()).expect("independent ledger handle");
        let wake_id = wake_id.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            claim_at(
                &ledger,
                &wake_id,
                &opaque_ref("machine", machine),
                at(0),
                at(30),
            )
        }));
    }
    barrier.wait();
    let results = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let connection = ledger.connect_read_only().expect("connection");
    assert_eq!(
        count_where(&connection, "outbox", "state", "claimed").expect("claimed"),
        1
    );
}

#[test]
fn expired_unstarted_claim_requeues_with_monotonic_attempt() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let first = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m1"),
        at(0),
        at(10),
    )
    .expect("first claim");
    assert_eq!(
        reconcile_expired_at(&ledger, &wake_id, at(11), &digest(b"restart")).expect("requeue"),
        ExpiredClaimDisposition::RequeuedUnstarted
    );
    let second = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m5"),
        at(12),
        at(22),
    )
    .expect("second claim");
    assert_eq!(first.claim_attempt, 1);
    assert_eq!(second.claim_attempt, 2);
    assert_ne!(first.claim_id, second.claim_id);
    assert_ne!(first.identity_digest, second.identity_digest);
}

#[test]
fn started_expiry_is_uncertain_and_never_requeued() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(10),
    )
    .expect("claim");
    start_at(&ledger, &claim, at(1)).expect("started boundary");
    assert_eq!(
        reconcile_expired_at(&ledger, &wake_id, at(11), &digest(b"restart ambiguity"),)
            .expect("uncertain"),
        ExpiredClaimDisposition::MarkedUncertain
    );
    assert!(
        claim_at(
            &ledger,
            &wake_id,
            &opaque_ref("machine", "m1"),
            at(12),
            at(22),
        )
        .is_err()
    );
    let connection = ledger.connect_read_only().expect("connection");
    let row: (String, String, String) = connection
        .query_row(
            "SELECT state, receipt_kind, receipt_digest FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("uncertain row");
    assert_eq!(row.0, "uncertain");
    assert_eq!(row.1, "uncertain");
    validate_digest("receipt", &row.2).expect("opaque receipt digest");
}

#[test]
fn uncertain_delivery_acceptance_transfers_ownership_without_retry() {
    let (temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(10),
    )
    .expect("claim");
    let started = start_at(&ledger, &claim, at(1)).expect("started boundary");
    let delivery_start_digest = started.start_identity_digest;
    reconcile_expired_at(&ledger, &wake_id, at(11), &digest(b"restart ambiguity"))
        .expect("uncertain");
    drop(claim);
    drop(ledger);
    let ledger = WorkLedger::open(temp.path()).expect("restart ledger");
    let claim = ledger
        .recover_uncertain_claim(&wake_id)
        .expect("recover exact durable claim");
    let receipt = DeliveryReceipt::accepted_after_uncertainty(
        &claim,
        &delivery_start_digest,
        claim.route.native_session_ref.clone(),
        digest(b"late exact acceptance"),
    )
    .expect("accepted receipt");

    reconcile_uncertain_at(&ledger, &claim, &receipt, at(12)).expect("resolve accepted");

    let connection = ledger.connect_read_only().expect("connection");
    let row: (String, String) = connection
        .query_row(
            "SELECT state, receipt_kind FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("resolved wake");
    assert_eq!(row, ("acknowledged".to_owned(), "accepted".to_owned()));
    let phase: String = connection
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "agent_owned_repair");
}

#[test]
fn uncertain_non_delivery_returns_actionable_but_never_pending() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(10),
    )
    .expect("claim");
    let started = start_at(&ledger, &claim, at(1)).expect("started boundary");
    reconcile_expired_at(&ledger, &wake_id, at(11), &digest(b"restart ambiguity"))
        .expect("uncertain");
    let receipt = DeliveryReceipt::not_delivered_after_uncertainty(
        &claim,
        &started.start_identity_digest,
        &digest(b"provider proves no delivery"),
    )
    .expect("not-delivered receipt");

    reconcile_uncertain_at(&ledger, &claim, &receipt, at(12)).expect("resolve not delivered");

    let connection = ledger.connect_read_only().expect("connection");
    let row: (String, String) = connection
        .query_row(
            "SELECT state, receipt_kind FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("resolved wake");
    assert_eq!(
        row,
        ("failed".to_owned(), "reconciled_not_delivered".to_owned())
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending"),
        0
    );
    let phase: String = connection
        .query_row(
            "SELECT phase FROM work_items WHERE id = ?1",
            [&work_id],
            |row| row.get(0),
        )
        .expect("phase");
    assert_eq!(phase, "actionable");
}

#[test]
fn exact_receipt_acknowledges_and_transfers_repair_ownership_atomically() {
    let (_temp, ledger, work_id, wake_id, adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let started = start_at(&ledger, &claim, at(1)).expect("started");
    assert!(
        DeliveryReceipt::new(
            &started,
            opaque_ref("session", "different session"),
            digest(b"wrong adapter receipt"),
        )
        .is_err(),
        "accepted receipt must prove the exact claimed native session"
    );
    let receipt = DeliveryReceipt::new(
        &started,
        started.claim.route.native_session_ref.clone(),
        digest(b"adapter receipt"),
    )
    .expect("receipt");
    let mut wrong_started = started.clone();
    wrong_started.claim.claim_attempt += 1;
    assert!(acknowledge_at(&ledger, &wrong_started, &receipt, at(2)).is_err());
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE adapter_registry SET state = 'retired' WHERE registry_ref = ?1",
            [adapter.registry_ref.as_str()],
        )
        .expect("retire after accepted delivery");
    drop(connection);
    acknowledge_at(&ledger, &started, &receipt, at(2)).expect("acknowledge exact receipt");
    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work");
    assert_eq!(work, ("agent_owned_repair".to_owned(), 7));
    let outbox: (String, String) = connection
        .query_row(
            "SELECT state, receipt_kind FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("outbox");
    assert_eq!(outbox, ("acknowledged".to_owned(), "accepted".to_owned()));
}

#[test]
fn definitive_pre_delivery_failure_returns_to_actionable_with_receipt() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    assert!(
        ledger
            .transition_with_wake(&work_id, 6, 3, LifecycleState::Actionable, None)
            .is_err(),
        "generic lifecycle mutation must not bypass the definitive receipt"
    );
    fail_unstarted_at(&ledger, &claim, &digest(b"quota unavailable"), at(1))
        .expect("definitive failure");
    let connection = ledger.connect_read_only().expect("connection");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work");
    assert_eq!(work, ("actionable".to_owned(), 7));
    let outbox: (String, String, String) = connection
        .query_row(
            "SELECT state, receipt_kind, receipt_digest FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("outbox");
    assert_eq!(outbox.0, "failed");
    assert_eq!(outbox.1, "definitive_pre_delivery_failure");
    validate_digest("receipt", &outbox.2).expect("receipt digest");
}

#[test]
fn adapter_drift_after_claim_refuses_delivery_start() {
    let (_temp, ledger, _work_id, wake_id, adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE adapter_registry SET implementation_digest = ?1
             WHERE registry_ref = ?2",
            params![
                digest(b"drifted implementation"),
                adapter.registry_ref.as_str()
            ],
        )
        .expect("drift adapter");
    drop(connection);
    assert!(start_at(&ledger, &claim, at(1)).is_err());
    let connection = ledger.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("state");
    assert_eq!(state, "claimed");
}

#[test]
fn acknowledgment_event_failure_rolls_back_receipt_and_work_transition() {
    let (_temp, ledger, work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let started = start_at(&ledger, &claim, at(1)).expect("start");
    let receipt = DeliveryReceipt::new(
        &started,
        started.claim.route.native_session_ref.clone(),
        digest(b"transport evidence"),
    )
    .expect("receipt");
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_wake_ack_event
             BEFORE INSERT ON events WHEN NEW.kind = 'wake_acknowledged'
             BEGIN SELECT RAISE(ABORT, 'event failure'); END;",
        )
        .expect("trigger");
    drop(connection);
    assert!(acknowledge_at(&ledger, &started, &receipt, at(2)).is_err());
    let connection = ledger.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("outbox state");
    assert_eq!(state, "delivery_started");
    let work: (String, u64) = connection
        .query_row(
            "SELECT phase, work_generation FROM work_items WHERE id = ?1",
            [&work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("work");
    assert_eq!(work, ("dispatching".to_owned(), 6));
}

#[test]
fn outbox_shape_constraints_reject_untyped_delivery_state() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let connection = ledger.connect_read_write().expect("connection");
    assert!(
        connection
            .execute(
                "UPDATE outbox SET state = 'delivery_started' WHERE wake_id = ?1",
                [&wake_id],
            )
            .is_err()
    );
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("state");
    assert_eq!(state, "pending");
}

fn assert_terminal_receipt_kind_is_required(ledger: &WorkLedger, wake_id: &str) {
    let connection = ledger.connect_read_write().expect("connection");
    for (state, delivery_started_at) in [
        ("acknowledged", Some("2026-08-28T12:00:01Z")),
        ("uncertain", Some("2026-08-28T12:00:01Z")),
        ("failed", None),
        ("failed", Some("2026-08-28T12:00:01Z")),
    ] {
        let delivery_start_digest = delivery_started_at.map(|_| digest(b"start"));
        let result = connection.execute(
            "UPDATE outbox SET state = ?1,
                    claim_id = 'claim', claimant_ref = 'opaque:sha256:claimant',
                    claim_attempt = 1, claim_identity_digest = 'identity',
                    claim_payload_json = x'7b7d',
                    claimed_at = '2026-08-28T12:00:00Z',
                    lease_expires_at = '2026-08-28T12:00:30Z',
                    dispatcher_epoch_ref = ?2, delivery_started_at = ?3,
                    delivery_start_digest = ?4, receipt_kind = NULL,
                    receipt_digest = 'receipt',
                    completed_at = '2026-08-28T12:00:02Z'
              WHERE wake_id = ?5",
            params![
                state,
                opaque_ref("dispatcher", "terminal shape"),
                delivery_started_at,
                delivery_start_digest,
                wake_id,
            ],
        );
        assert!(result.is_err(), "{state} must reject a NULL receipt kind");
    }
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [wake_id],
            |row| row.get(0),
        )
        .expect("state after rejected terminal writes");
    assert_eq!(state, "pending");
}

#[test]
fn fresh_v4_outbox_rejects_null_receipt_kind_for_every_terminal_state() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    assert_terminal_receipt_kind_is_required(&ledger, &wake_id);
}

#[test]
fn wake_identity_changes_across_ledger_incarnations() {
    let first_temp = TempDir::new().expect("first temp");
    let second_temp = TempDir::new().expect("second temp");
    let first = WorkLedger::open(first_temp.path()).expect("first ledger");
    let second = WorkLedger::open(second_temp.path()).expect("second ledger");
    let work_id = opaque_ref("wi", "same work");
    let route_ref = opaque_ref("route", "same route");
    let payload = digest(b"same payload");
    let first_wake = first
        .wake_intent(&work_id, 6, 3, route_ref.clone(), payload.clone())
        .expect("first wake");
    let second_wake = second
        .wake_intent(&work_id, 6, 3, route_ref, payload)
        .expect("second wake");
    assert_ne!(
        first_wake.ledger_incarnation_ref,
        second_wake.ledger_incarnation_ref
    );
    assert_ne!(first_wake.wake_id, second_wake.wake_id);
}

#[test]
fn dispatcher_restart_changes_epoch_and_stale_claim_cannot_cross() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let first_epoch = ledger.dispatcher_epoch().expect("first epoch");
    let first = claim_in_epoch_at(
        &ledger,
        &first_epoch,
        &wake_id,
        &opaque_ref("machine", "m1"),
        at(0),
        at(10),
    )
    .expect("first claim");
    reconcile_expired_at(&ledger, &wake_id, at(11), &digest(b"restart"))
        .expect("requeue unstarted claim");
    let second_epoch = ledger.dispatcher_epoch().expect("second epoch");
    let second = claim_in_epoch_at(
        &ledger,
        &second_epoch,
        &wake_id,
        &opaque_ref("machine", "m1"),
        at(12),
        at(22),
    )
    .expect("second claim");
    assert_ne!(first.dispatcher_epoch_ref, second.dispatcher_epoch_ref);
    assert_ne!(first.claim_id, second.claim_id);
    assert!(start_at(&ledger, &first, at(13)).is_err());
}

#[test]
fn start_receipt_outbox_and_events_bind_both_incarnations() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let dispatcher = ledger.dispatcher_epoch().expect("dispatcher epoch");
    let claim = claim_in_epoch_at(
        &ledger,
        &dispatcher,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let started = start_at(&ledger, &claim, at(1)).expect("start");
    let receipt = DeliveryReceipt::new(
        &started,
        claim.route.native_session_ref.clone(),
        digest(b"accepted"),
    )
    .expect("receipt");
    assert_eq!(
        claim.ledger_incarnation_ref,
        ledger.ledger_incarnation_ref()
    );
    assert_eq!(claim.dispatcher_epoch_ref, dispatcher.dispatcher_epoch_ref);
    assert_eq!(
        receipt.delivery_start_digest.as_deref(),
        Some(started.start_identity_digest.as_str())
    );

    let connection = ledger.connect_read_only().expect("connection");
    let stored: (String, String, String) = connection
        .query_row(
            "SELECT ledger_incarnation_ref, dispatcher_epoch_ref, delivery_start_digest
             FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("stored incarnation fences");
    assert_eq!(stored.0, claim.ledger_incarnation_ref);
    assert_eq!(stored.1, claim.dispatcher_epoch_ref);
    assert_eq!(stored.2, started.start_identity_digest);
    let event_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM events
             WHERE ledger_incarnation_ref = ?1 AND dispatcher_epoch_ref = ?2
               AND kind IN ('wake_claimed', 'wake_delivery_started')",
            params![claim.ledger_incarnation_ref, claim.dispatcher_epoch_ref],
            |row| row.get(0),
        )
        .expect("event fences");
    assert_eq!(event_count, 2);
}

#[test]
fn v3_active_outbox_refuses_incarnation_migration_without_mutation() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("active legacy-shaped claim");
    super::persistence::strip_v4_incarnation_schema(&ledger);
    drop(ledger);

    assert!(matches!(
        WorkLedger::open(temp.path()),
        Err(WorkLedgerError::Refused(reason))
            if reason.contains("dispatcher epoch provenance")
    ));
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v3");
    assert_eq!(schema_version(&connection).expect("version"), 3);
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved active wake");
    assert_eq!(state, "claimed");
    let metadata_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'ledger_metadata')",
            [],
            |row| row.get(0),
        )
        .expect("metadata absence");
    assert!(!metadata_exists);
}

#[test]
fn failed_v3_incarnation_upgrade_rolls_back_schema_and_pending_wake() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    super::persistence::strip_v4_incarnation_schema(&ledger);
    let connection = ledger.connect_read_write().expect("v3 connection");
    connection
        .execute_batch("CREATE TABLE ledger_metadata (collision TEXT);")
        .expect("migration collision");
    drop(connection);
    drop(ledger);

    assert!(WorkLedger::open(temp.path()).is_err());
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v3");
    assert_eq!(schema_version(&connection).expect("version"), 3);
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved pending wake");
    assert_eq!(state, "pending");
}

fn install_exact_v2_outbox(ledger: &WorkLedger) {
    super::persistence::strip_v4_incarnation_schema(ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch(
            "DROP INDEX outbox_delivery;
             ALTER TABLE outbox RENAME TO outbox_v3;
             CREATE TABLE outbox (
               wake_id TEXT PRIMARY KEY,
               work_item_id TEXT NOT NULL REFERENCES work_items(id) ON DELETE RESTRICT,
               work_generation INTEGER NOT NULL,
               owner_generation INTEGER NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('pending', 'claimed', 'acknowledged', 'uncertain', 'failed')),
               route_ref TEXT NOT NULL,
               payload_digest TEXT NOT NULL,
               transport_receipt_digest TEXT,
               created_at TEXT NOT NULL,
               updated_at TEXT NOT NULL,
               acknowledged_at TEXT
             );
             INSERT INTO outbox
               (wake_id, work_item_id, work_generation, owner_generation, state,
                route_ref, payload_digest, created_at, updated_at)
             SELECT wake_id, work_item_id, work_generation, owner_generation, state,
                    route_ref, payload_digest, created_at, updated_at FROM outbox_v3;
             DROP TABLE outbox_v3;
             CREATE INDEX outbox_delivery ON outbox(state, created_at, wake_id);
             PRAGMA user_version = 2;",
        )
        .expect("exact v2 outbox");
}

#[test]
fn v2_pending_outbox_migrates_to_v4_without_losing_wake() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    drop(ledger);
    let migrated = WorkLedger::open(temp.path()).expect("migrate v2");
    let connection = migrated.connect_read_only().expect("connection");
    assert_eq!(schema_version(&connection).expect("version"), 7);
    let row: (String, u64, Option<String>, String) = connection
        .query_row(
            "SELECT state, claim_attempt, claim_id, ledger_incarnation_ref
             FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("preserved wake");
    assert_eq!(row.0, "pending");
    assert_eq!(row.1, 0);
    assert_eq!(row.2, None);
    assert_eq!(row.3, migrated.ledger_incarnation_ref());
    assert_terminal_receipt_kind_is_required(&migrated, &wake_id);
}

#[test]
fn full_v1_pending_outbox_migrates_to_v4_with_terminal_receipt_constraints() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute("DELETE FROM route_records", [])
        .expect("remove route records from the exact positive v1 fixture");
    drop(connection);
    super::persistence::install_exact_v1_registry_schema(&ledger, &[]);
    drop(ledger);

    let migrated = WorkLedger::open(temp.path()).expect("migrate full v1 ledger");
    let connection = migrated.connect_read_only().expect("connection");
    assert_eq!(schema_version(&connection).expect("version"), 7);
    let row: (String, u64, Option<String>, String) = connection
        .query_row(
            "SELECT state, claim_attempt, claim_id, ledger_incarnation_ref
             FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("preserved v1 wake");
    assert_eq!(row.0, "pending");
    assert_eq!(row.1, 0);
    assert_eq!(row.2, None);
    assert_eq!(row.3, migrated.ledger_incarnation_ref());
    drop(connection);
    assert_terminal_receipt_kind_is_required(&migrated, &wake_id);
}

#[test]
fn v2_nonpending_outbox_refuses_migration_without_mutation() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute(
            "UPDATE outbox SET state = 'uncertain' WHERE wake_id = ?1",
            [&wake_id],
        )
        .expect("legacy nonpending row");
    drop(connection);
    drop(ledger);
    assert!(matches!(
        WorkLedger::open(temp.path()),
        Err(WorkLedgerError::Refused(reason))
            if reason.contains("explicit outbox reconciliation")
    ));
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v2");
    assert_eq!(schema_version(&connection).expect("version"), 2);
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved state");
    assert_eq!(state, "uncertain");
}

#[test]
fn failed_v2_outbox_rebuild_rolls_back_schema_and_pending_wake() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    install_exact_v2_outbox(&ledger);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch("CREATE TABLE outbox_v2 (collision TEXT);")
        .expect("migration collision");
    drop(connection);
    drop(ledger);
    assert!(WorkLedger::open(temp.path()).is_err());
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v2");
    assert_eq!(schema_version(&connection).expect("version"), 2);
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("preserved wake");
    assert_eq!(state, "pending");
}

#[test]
fn failed_second_stage_v1_to_v3_upgrade_rolls_back_registry_and_version() {
    let temp = TempDir::new().expect("temp");
    let ledger = WorkLedger::open(temp.path()).expect("current ledger");
    install_exact_v2_outbox(&ledger);
    let terminal_adapter = adapter_binding(AdapterAxis::Terminal, "wezterm", "wezterm");
    super::persistence::install_exact_v1_registry_schema(&ledger, &[(&terminal_adapter, "active")]);
    let connection = ledger.connect_read_write().expect("connection");
    connection
        .execute_batch("CREATE TABLE outbox_v2 (collision TEXT);")
        .expect("second-stage collision");
    drop(connection);
    drop(ledger);

    assert!(WorkLedger::open(temp.path()).is_err());
    let connection = Connection::open(WorkLedger::path_at(temp.path())).expect("inspect v1");
    assert_eq!(schema_version(&connection).expect("version"), 1);
    let registry_sql: String = connection
        .query_row(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'adapter_registry'",
            [],
            |row| row.get(0),
        )
        .expect("registry schema");
    assert!(registry_sql.contains("'terminal', 'provider'"));
    assert!(!registry_sql.contains("'agent'"));
    let rows: i64 = connection
        .query_row("SELECT COUNT(*) FROM adapter_registry", [], |row| {
            row.get(0)
        })
        .expect("preserved registry");
    assert_eq!(rows, 1);
}

#[test]
fn delivery_records_only_opaque_identity_and_uses_no_external_callback() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "raw-machine-secret"),
        at(0),
        at(30),
    )
    .expect("pure ledger claim");
    let encoded = serde_json::to_string(&claim).expect("claim JSON");
    for forbidden in [
        "raw-machine-secret",
        "secret-account",
        "resume-private-id",
        "owner-private-id",
    ] {
        assert!(!encoded.contains(forbidden), "claim leaked {forbidden}");
        for suffix in ["", "-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{}", ledger.path().display(), suffix));
            if path.exists() {
                let bytes = fs::read(path).expect("ledger bytes");
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(forbidden),
                    "ledger leaked {forbidden}"
                );
            }
        }
    }
}

#[test]
fn claim_lease_is_bounded() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    assert!(
        claim_at(
            &ledger,
            &wake_id,
            &opaque_ref("machine", "m3"),
            at(0),
            at(0) + ChronoDuration::minutes(6),
        )
        .is_err()
    );
}

#[test]
fn zero_and_over_five_minute_leases_refuse_before_clock_consumption() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let dispatcher = ledger.dispatcher_epoch().expect("dispatcher epoch");
    set_time(&ledger, at(0));
    assert!(
        ledger
            .claim_wake_in_epoch(
                &dispatcher,
                &wake_id,
                &opaque_ref("machine", "m3"),
                Duration::ZERO,
            )
            .is_err()
    );
    assert!(
        ledger
            .claim_wake_in_epoch(
                &dispatcher,
                &wake_id,
                &opaque_ref("machine", "m3"),
                Duration::from_secs(301),
            )
            .is_err()
    );

    set_time(&ledger, regressed_wall());
    let claim = ledger
        .claim_wake_in_epoch(
            &dispatcher,
            &wake_id,
            &opaque_ref("machine", "m3"),
            Duration::from_secs(30),
        )
        .expect("invalid leases must not advance the owned clock");
    assert_eq!(claim.claimed_at, regressed_wall());
}

#[test]
fn clock_is_shared_monotonic_across_cloned_ledger_handles() {
    let (_temp, ledger, _work_id, _wake_id, _adapter) = pending_delivery();
    let clone = ledger.clone();
    set_time(&ledger, at(10));
    let mut connection = ledger.connect_read_write().expect("connection");
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("transaction");
    let first = ledger
        .clock
        .observe(&transaction)
        .expect("first observation");
    set_time(&clone, at(9));
    let second = clone
        .clock
        .observe(&transaction)
        .expect("second observation");

    assert_eq!(first.timestamp, at(10));
    assert_eq!(second.timestamp, at(10));
    assert!(!second.restart_wall_regressed);
    transaction.rollback().expect("test rollback");
}

#[test]
fn in_process_wall_correction_clamps_without_premature_reconciliation() {
    let (_temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");

    set_time(&ledger, regressed_wall());
    assert!(
        ledger
            .reconcile_expired_claim(&wake_id, &digest(b"not a restart"))
            .is_err(),
        "an in-process correction must not bypass the live lease"
    );
    assert!(
        ledger.mark_delivery_started(&claim).is_err(),
        "new external work remains refused while wall time trails durable time"
    );
    let connection = ledger.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("claim state");
    assert_eq!(state, "claimed");
}

#[test]
fn independent_handle_refreshes_durable_floor_before_delivery_start() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let stale_handle = WorkLedger::open(temp.path()).expect("second handle before claim");
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim through first handle");

    set_time(&stale_handle, regressed_wall());
    assert!(
        stale_handle.mark_delivery_started(&claim).is_err(),
        "storage drift must refresh the durable floor before mutation"
    );
}

#[test]
fn restart_wall_regression_refuses_new_claim_and_delivery_start() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    drop(ledger);

    let restarted = WorkLedger::open(temp.path()).expect("restart ledger");
    set_time(&restarted, regressed_wall());
    assert!(restarted.mark_delivery_started(&claim).is_err());
    let connection = restarted.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("claim state");
    assert_eq!(state, "claimed");

    drop(connection);
    reconcile_expired_at(
        &restarted,
        &wake_id,
        regressed_wall(),
        &digest(b"restart regression"),
    )
    .expect("contain unstarted claim");
    assert!(
        claim_at(
            &restarted,
            &wake_id,
            &opaque_ref("machine", "m5"),
            regressed_wall(),
            regressed_wall() + ChronoDuration::seconds(30),
        )
        .is_err()
    );
}

#[test]
fn restart_wall_regression_requeues_unstarted_before_lease_expiry() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    drop(ledger);

    let restarted = WorkLedger::open(temp.path()).expect("restart ledger");
    assert_eq!(
        reconcile_expired_at(
            &restarted,
            &wake_id,
            regressed_wall(),
            &digest(b"restart regression"),
        )
        .expect("immediate containment"),
        ExpiredClaimDisposition::RequeuedUnstarted
    );
    let connection = restarted.connect_read_only().expect("connection");
    let state: String = connection
        .query_row(
            "SELECT state FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| row.get(0),
        )
        .expect("outbox state");
    assert_eq!(state, "pending");
}

#[test]
fn restart_wall_regression_marks_started_uncertain_before_lease_expiry() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    start_at(&ledger, &claim, at(1)).expect("start");
    drop(ledger);

    let restarted = WorkLedger::open(temp.path()).expect("restart ledger");
    assert_eq!(
        reconcile_expired_at(
            &restarted,
            &wake_id,
            regressed_wall(),
            &digest(b"restart ambiguity"),
        )
        .expect("immediate containment"),
        ExpiredClaimDisposition::MarkedUncertain
    );
    assert!(
        claim_at(
            &restarted,
            &wake_id,
            &opaque_ref("machine", "m5"),
            regressed_wall(),
            regressed_wall() + ChronoDuration::seconds(30),
        )
        .is_err()
    );
    let connection = restarted.connect_read_only().expect("connection");
    assert_eq!(
        count_where(&connection, "outbox", "state", "uncertain").expect("uncertain"),
        1
    );
    assert_eq!(
        count_where(&connection, "outbox", "state", "pending").expect("pending"),
        0
    );
}

#[test]
fn terminal_containment_commits_at_durable_floor_during_regression() {
    let (temp, ledger, _work_id, wake_id, _adapter) = pending_delivery();
    let claim = claim_at(
        &ledger,
        &wake_id,
        &opaque_ref("machine", "m3"),
        at(0),
        at(30),
    )
    .expect("claim");
    let started = start_at(&ledger, &claim, at(1)).expect("start");
    let receipt = DeliveryReceipt::new(
        &started,
        started.claim.route.native_session_ref.clone(),
        digest(b"accepted during regression"),
    )
    .expect("receipt");
    drop(ledger);

    let restarted = WorkLedger::open(temp.path()).expect("restart ledger");
    set_time(&restarted, regressed_wall());
    restarted
        .acknowledge_delivery(&started, &receipt)
        .expect("terminal containment at durable floor");
    let connection = restarted.connect_read_only().expect("connection");
    let (state, completed_at): (String, String) = connection
        .query_row(
            "SELECT state, completed_at FROM outbox WHERE wake_id = ?1",
            [&wake_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("completed outbox");
    assert_eq!(state, "acknowledged");
    assert_eq!(
        chrono::DateTime::parse_from_rfc3339(&completed_at)
            .expect("timestamp")
            .with_timezone(&Utc),
        started.started_at
    );
}
