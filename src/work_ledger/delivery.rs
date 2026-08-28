//! Transactional, transport-neutral wake delivery.
//!
//! This module deliberately performs no external calls. It makes the durable
//! boundary around such a call explicit so a later host adapter can never
//! confuse an unstarted claim with an ambiguously delivered wake.
#![allow(dead_code)] // Activated only after host-adapter canaries.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

use super::lifecycle::record_event;
use super::registry::validated_route_exists;
use super::route::{
    AgentRoute, AgentRouteRecord, NativeSessionRoute, ProviderRoute, RouteProvenanceRecord,
    TerminalRoute,
};
use super::{
    LifecycleState, OptionalExtension, Transaction, TransactionBehavior, WorkLedger,
    WorkLedgerError, WorkLedgerResult, configure_durable, digest, opaque_ref, params,
    validate_digest, validate_opaque_ref, verify_integrity, verify_supported_schema,
};

const DELIVERY_SCHEMA_VERSION: u32 = 1;
const MAX_CLAIM_LEASE: ChronoDuration = ChronoDuration::minutes(5);

/// Opaque, exact route identity required by a transport adapter.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeliveryRouteIdentity {
    pub(super) terminal_kind: String,
    pub(super) terminal: TerminalRoute,
    pub(super) agent_kind: String,
    pub(super) agent: AgentRouteRecord,
    pub(super) provider_kind: String,
    pub(super) provider: ProviderRoute,
    pub(super) route_revision: u64,
    pub(super) route_integrity: String,
    pub(super) native_session_ref: String,
    pub(super) native_resume_ref: String,
    pub(super) account_ref: String,
    pub(super) model_ref: String,
    pub(super) wrapper_ref: String,
    pub(super) session_headers_ref: String,
    pub(super) session_headers_sha256: String,
    pub(super) launch_profile_ref: String,
    pub(super) launch_generation: u64,
    pub(super) launch_revision: u64,
    pub(super) executable_sha256: String,
    pub(super) configuration_sha256: String,
}

/// Singleton claim returned before any external delivery is attempted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeliveryClaim {
    pub(super) schema_version: u32,
    pub(super) wake_id: String,
    pub(super) claim_id: String,
    pub(super) claimant_ref: String,
    pub(super) claim_attempt: u64,
    pub(super) work_id: String,
    pub(super) head_sha: String,
    pub(super) work_generation: u64,
    pub(super) owner_generation: u64,
    pub(super) route_ref: String,
    pub(super) payload_digest: String,
    pub(super) claimed_at: DateTime<Utc>,
    pub(super) claim_expires_at: DateTime<Utc>,
    pub(super) route: DeliveryRouteIdentity,
    pub(super) identity_digest: String,
}

/// Token proving Shipyard committed the possible-external-delivery boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StartedDelivery {
    pub(super) claim: DeliveryClaim,
    pub(super) started_at: DateTime<Utc>,
}

/// Exact, opaque receipt returned only after an adapter accepted a wake.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DeliveryReceipt {
    schema_version: u32,
    wake_id: String,
    claim_id: String,
    delivery_identity_digest: String,
    receipt_kind: DeliveryReceiptKind,
    observed_native_session_ref: Option<String>,
    transport_evidence_digest: String,
    receipt_integrity: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum DeliveryReceiptKind {
    Accepted,
    DefinitivePreDeliveryFailure,
    ReconciledNotDelivered,
    Uncertain,
}

impl DeliveryReceiptKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::DefinitivePreDeliveryFailure => "definitive_pre_delivery_failure",
            Self::ReconciledNotDelivered => "reconciled_not_delivered",
            Self::Uncertain => "uncertain",
        }
    }
}

/// Deterministic restart handling for an expired durable claim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExpiredClaimDisposition {
    /// No external call could have occurred, so the wake is claimable again.
    RequeuedUnstarted,
    /// An external call may have occurred, so the wake requires reconciliation.
    MarkedUncertain,
}

#[derive(Clone, Debug)]
struct PendingWake {
    wake_id: String,
    work_id: String,
    work_generation: u64,
    owner_generation: u64,
    route_ref: String,
    payload_digest: String,
    claim_attempt: u64,
}

struct ExpiredClaimRecord {
    claim_id: String,
    claim_identity: String,
    expires_at: DateTime<Utc>,
    state: String,
    started_at: Option<String>,
    work_id: String,
    work_generation: u64,
    owner_generation: u64,
}

struct StoredRouteRow {
    head: String,
    owner: String,
    owner_generation: u64,
    revision: u64,
    payload: Vec<u8>,
    integrity: String,
    terminal_kind: String,
    agent_kind: String,
    provider_kind: String,
}

impl DeliveryClaim {
    fn finalize(mut self) -> WorkLedgerResult<Self> {
        self.identity_digest.clear();
        let encoded = serde_json::to_vec(&self).map_err(|_| {
            WorkLedgerError::Refused("delivery claim cannot be serialized".to_owned())
        })?;
        self.identity_digest = digest(&encoded);
        Ok(self)
    }

    fn validate_identity(&self) -> WorkLedgerResult<()> {
        if self.schema_version != DELIVERY_SCHEMA_VERSION {
            return Err(WorkLedgerError::Refused(
                "unsupported delivery claim schema".to_owned(),
            ));
        }
        let expected = self.clone().finalize()?.identity_digest;
        if expected != self.identity_digest {
            return Err(WorkLedgerError::Refused(
                "delivery claim identity is invalid".to_owned(),
            ));
        }
        Ok(())
    }
}

impl DeliveryReceipt {
    pub(super) fn new(
        started: &StartedDelivery,
        observed_native_session_ref: String,
        transport_evidence_digest: String,
    ) -> WorkLedgerResult<Self> {
        Self::accepted_for_claim(
            &started.claim,
            observed_native_session_ref,
            transport_evidence_digest,
        )
    }

    /// Build exact acceptance evidence recovered after an uncertain restart.
    pub(super) fn accepted_after_uncertainty(
        claim: &DeliveryClaim,
        observed_native_session_ref: String,
        transport_evidence_digest: String,
    ) -> WorkLedgerResult<Self> {
        Self::accepted_for_claim(
            claim,
            observed_native_session_ref,
            transport_evidence_digest,
        )
    }

    /// Build proof that an uncertain external attempt was definitively not delivered.
    pub(super) fn not_delivered_after_uncertainty(
        claim: &DeliveryClaim,
        transport_evidence_digest: &str,
    ) -> WorkLedgerResult<Self> {
        Self::outcome(
            claim,
            DeliveryReceiptKind::ReconciledNotDelivered,
            transport_evidence_digest,
        )
    }

    fn accepted_for_claim(
        claim: &DeliveryClaim,
        observed_native_session_ref: String,
        transport_evidence_digest: String,
    ) -> WorkLedgerResult<Self> {
        claim.validate_identity()?;
        if observed_native_session_ref != claim.route.native_session_ref {
            return Err(WorkLedgerError::Refused(
                "adapter accepted a different native session than the claimed route".to_owned(),
            ));
        }
        validate_digest("transport evidence", &transport_evidence_digest)?;
        let mut receipt = Self {
            schema_version: DELIVERY_SCHEMA_VERSION,
            wake_id: claim.wake_id.clone(),
            claim_id: claim.claim_id.clone(),
            delivery_identity_digest: claim.identity_digest.clone(),
            receipt_kind: DeliveryReceiptKind::Accepted,
            observed_native_session_ref: Some(observed_native_session_ref),
            transport_evidence_digest,
            receipt_integrity: String::new(),
        };
        receipt.receipt_integrity = receipt.recompute_integrity()?;
        Ok(receipt)
    }

    fn outcome(
        claim: &DeliveryClaim,
        receipt_kind: DeliveryReceiptKind,
        evidence_digest: &str,
    ) -> WorkLedgerResult<Self> {
        claim.validate_identity()?;
        validate_digest("delivery outcome evidence", evidence_digest)?;
        if receipt_kind == DeliveryReceiptKind::Accepted {
            return Err(WorkLedgerError::Refused(
                "accepted delivery requires an observed native session".to_owned(),
            ));
        }
        let mut receipt = Self {
            schema_version: DELIVERY_SCHEMA_VERSION,
            wake_id: claim.wake_id.clone(),
            claim_id: claim.claim_id.clone(),
            delivery_identity_digest: claim.identity_digest.clone(),
            receipt_kind,
            observed_native_session_ref: None,
            transport_evidence_digest: evidence_digest.to_owned(),
            receipt_integrity: String::new(),
        };
        receipt.receipt_integrity = receipt.recompute_integrity()?;
        Ok(receipt)
    }

    fn recompute_integrity(&self) -> WorkLedgerResult<String> {
        #[derive(Serialize)]
        struct ReceiptIntegrity<'a> {
            schema_version: u32,
            wake_id: &'a str,
            claim_id: &'a str,
            delivery_identity_digest: &'a str,
            receipt_kind: DeliveryReceiptKind,
            observed_native_session_ref: Option<&'a str>,
            transport_evidence_digest: &'a str,
        }
        let bytes = serde_json::to_vec(&ReceiptIntegrity {
            schema_version: self.schema_version,
            wake_id: &self.wake_id,
            claim_id: &self.claim_id,
            delivery_identity_digest: &self.delivery_identity_digest,
            receipt_kind: self.receipt_kind,
            observed_native_session_ref: self.observed_native_session_ref.as_deref(),
            transport_evidence_digest: &self.transport_evidence_digest,
        })
        .map_err(|_| {
            WorkLedgerError::Refused("delivery receipt cannot be serialized".to_owned())
        })?;
        Ok(digest(&bytes))
    }

    fn validate_for_started(&self, started: &StartedDelivery) -> WorkLedgerResult<()> {
        self.validate_for_claim(&started.claim, DeliveryReceiptKind::Accepted)
    }

    fn validate_for_claim(
        &self,
        claim: &DeliveryClaim,
        expected_kind: DeliveryReceiptKind,
    ) -> WorkLedgerResult<()> {
        let observed_session_is_valid = match expected_kind {
            DeliveryReceiptKind::Accepted => {
                self.observed_native_session_ref.as_deref()
                    == Some(claim.route.native_session_ref.as_str())
            }
            _ => self.observed_native_session_ref.is_none(),
        };
        if self.schema_version != DELIVERY_SCHEMA_VERSION
            || self.receipt_kind != expected_kind
            || !observed_session_is_valid
            || self.wake_id != claim.wake_id
            || self.claim_id != claim.claim_id
            || self.delivery_identity_digest != claim.identity_digest
            || self.recompute_integrity()? != self.receipt_integrity
        {
            return Err(WorkLedgerError::Refused(
                "delivery receipt does not match its exact claim".to_owned(),
            ));
        }
        Ok(())
    }
}

impl WorkLedger {
    /// Atomically claim one exact pending wake without contacting its adapter.
    pub(super) fn claim_wake(
        &self,
        wake_id: &str,
        claimant_ref: &str,
        claimed_at: DateTime<Utc>,
        claim_expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<DeliveryClaim> {
        validate_opaque_ref("wake_id", wake_id, "wake")?;
        validate_opaque_ref("claimant_ref", claimant_ref, "machine")?;
        let lease = claim_expires_at.signed_duration_since(claimed_at);
        if lease <= ChronoDuration::zero() || lease > MAX_CLAIM_LEASE {
            return Err(WorkLedgerError::Refused(
                "delivery claim lease must be positive and no longer than five minutes".to_owned(),
            ));
        }
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let pending = pending_wake(&transaction, wake_id)?;
        let attempt = pending.claim_attempt.checked_add(1).ok_or_else(|| {
            WorkLedgerError::Refused("delivery claim attempt is exhausted".to_owned())
        })?;
        let route = validated_delivery_route(&transaction, &pending)?;
        let claim_id = opaque_ref(
            "claim",
            &format!("{}\n{attempt}\n{claimant_ref}", pending.wake_id),
        );
        let claim = DeliveryClaim {
            schema_version: DELIVERY_SCHEMA_VERSION,
            wake_id: pending.wake_id,
            claim_id,
            claimant_ref: claimant_ref.to_owned(),
            claim_attempt: attempt,
            work_id: pending.work_id,
            head_sha: route.0,
            work_generation: pending.work_generation,
            owner_generation: pending.owner_generation,
            route_ref: pending.route_ref,
            payload_digest: pending.payload_digest,
            claimed_at,
            claim_expires_at,
            route: route.1,
            identity_digest: String::new(),
        }
        .finalize()?;
        let claim_payload = serde_json::to_vec(&claim).map_err(|_| {
            WorkLedgerError::Refused("delivery claim cannot be persisted".to_owned())
        })?;
        let changed = transaction.execute(
            "UPDATE outbox SET state = 'claimed', claim_id = ?1, claimant_ref = ?2,
                    claim_attempt = ?3, claim_identity_digest = ?4, claimed_at = ?5,
                    lease_expires_at = ?6, claim_payload_json = ?7, updated_at = ?5
             WHERE wake_id = ?8 AND state = 'pending' AND claim_attempt = ?9",
            params![
                claim.claim_id,
                claim.claimant_ref,
                claim.claim_attempt,
                claim.identity_digest,
                claim.claimed_at.to_rfc3339(),
                claim.claim_expires_at.to_rfc3339(),
                claim_payload,
                claim.wake_id,
                pending.claim_attempt,
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "wake is no longer pending".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &claim.work_id,
            claim.work_generation,
            claim.owner_generation,
            "wake_claimed",
            Some(LifecycleState::Dispatching),
            LifecycleState::Dispatching,
            &claim.identity_digest,
            &claim.claimed_at.to_rfc3339(),
        )?;
        transaction.commit()?;
        claim.validate_identity()?;
        Ok(claim)
    }

    /// Commit the no-return boundary immediately before an external adapter call.
    pub(super) fn mark_delivery_started(
        &self,
        claim: &DeliveryClaim,
        started_at: DateTime<Utc>,
    ) -> WorkLedgerResult<StartedDelivery> {
        claim.validate_identity()?;
        if started_at < claim.claimed_at || started_at > claim.claim_expires_at {
            return Err(WorkLedgerError::Refused(
                "delivery start is outside its claim lease".to_owned(),
            ));
        }
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_claim(&transaction, claim, "claimed")?;
        let pending = stored_wake_for_claim(claim);
        let (_, route) = validated_delivery_route(&transaction, &pending)?;
        if route != claim.route {
            return Err(WorkLedgerError::Refused(
                "delivery route changed after claim".to_owned(),
            ));
        }
        let changed = transaction.execute(
            "UPDATE outbox SET state = 'delivery_started', delivery_started_at = ?1,
                    updated_at = ?1
             WHERE wake_id = ?2 AND state = 'claimed' AND claim_id = ?3
               AND delivery_started_at IS NULL",
            params![started_at.to_rfc3339(), claim.wake_id, claim.claim_id],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "delivery start boundary was already crossed".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &claim.work_id,
            claim.work_generation,
            claim.owner_generation,
            "wake_delivery_started",
            Some(LifecycleState::Dispatching),
            LifecycleState::Dispatching,
            &claim.identity_digest,
            &started_at.to_rfc3339(),
        )?;
        transaction.commit()?;
        Ok(StartedDelivery {
            claim: claim.clone(),
            started_at,
        })
    }

    /// Atomically acknowledge exact adapter acceptance and transfer repair ownership.
    pub(super) fn acknowledge_delivery(
        &self,
        started: &StartedDelivery,
        receipt: &DeliveryReceipt,
        acknowledged_at: DateTime<Utc>,
    ) -> WorkLedgerResult<()> {
        started.claim.validate_identity()?;
        receipt.validate_for_started(started)?;
        if acknowledged_at < started.started_at {
            return Err(WorkLedgerError::Refused(
                "delivery acknowledgment predates delivery start".to_owned(),
            ));
        }
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_started(&transaction, started)?;
        // Exact route and adapter bindings were fenced immediately before the
        // external call. Later registry retirement cannot erase an accepted
        // delivery; the receipt binds the immutable claim while the work update
        // below still fences the exact head and generations.
        let now = acknowledged_at.to_rfc3339();
        let changed = transaction.execute(
            "UPDATE outbox SET state = 'acknowledged', receipt_kind = 'accepted',
                    receipt_digest = ?1, completed_at = ?2, updated_at = ?2
             WHERE wake_id = ?3 AND state = 'delivery_started' AND claim_id = ?4
               AND delivery_started_at = ?5",
            params![
                receipt.receipt_integrity,
                now,
                started.claim.wake_id,
                started.claim.claim_id,
                started.started_at.to_rfc3339(),
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "delivery claim is no longer acknowledgeable".to_owned(),
            ));
        }
        let transitioned = transaction.execute(
            "UPDATE work_items SET phase = 'agent_owned_repair',
                    work_generation = work_generation + 1, updated_at = ?1
             WHERE id = ?2 AND phase = 'dispatching' AND work_generation = ?3
               AND owner_generation = ?4 AND head_sha = ?5",
            params![
                now,
                started.claim.work_id,
                started.claim.work_generation,
                started.claim.owner_generation,
                started.claim.head_sha,
            ],
        )?;
        if transitioned != 1 {
            return Err(WorkLedgerError::Refused(
                "work changed before delivery acknowledgment".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &started.claim.work_id,
            started.claim.work_generation + 1,
            started.claim.owner_generation,
            "wake_acknowledged",
            Some(LifecycleState::Dispatching),
            LifecycleState::AgentOwnedRepair,
            &receipt.receipt_integrity,
            &now,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Persist ambiguity after the external-delivery boundary. Never retries.
    pub(super) fn mark_delivery_uncertain(
        &self,
        started: &StartedDelivery,
        uncertainty_digest: &str,
        observed_at: DateTime<Utc>,
    ) -> WorkLedgerResult<()> {
        validate_digest("uncertainty evidence", uncertainty_digest)?;
        started.claim.validate_identity()?;
        if observed_at < started.started_at {
            return Err(WorkLedgerError::Refused(
                "uncertainty evidence predates delivery start".to_owned(),
            ));
        }
        let receipt = DeliveryReceipt::outcome(
            &started.claim,
            DeliveryReceiptKind::Uncertain,
            uncertainty_digest,
        )?;
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_started(&transaction, started)?;
        update_uncertain(
            &transaction,
            &started.claim.wake_id,
            &started.claim.claim_id,
            &receipt.receipt_integrity,
            &observed_at.to_rfc3339(),
        )?;
        record_event(
            &transaction,
            &started.claim.work_id,
            started.claim.work_generation,
            started.claim.owner_generation,
            "wake_delivery_uncertain",
            Some(LifecycleState::Dispatching),
            LifecycleState::Dispatching,
            &receipt.receipt_integrity,
            &observed_at.to_rfc3339(),
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Resolve an uncertain delivery only from exact, typed transport evidence.
    /// A definitive non-delivery returns work to `actionable`; it never retries
    /// or rewrites the wake to `pending`.
    pub(super) fn reconcile_uncertain_delivery(
        &self,
        claim: &DeliveryClaim,
        receipt: &DeliveryReceipt,
        resolved_at: DateTime<Utc>,
    ) -> WorkLedgerResult<()> {
        claim.validate_identity()?;
        if receipt.receipt_kind != DeliveryReceiptKind::Accepted
            && receipt.receipt_kind != DeliveryReceiptKind::ReconciledNotDelivered
        {
            return Err(WorkLedgerError::Refused(
                "uncertain delivery requires a definitive typed resolution".to_owned(),
            ));
        }
        receipt.validate_for_claim(claim, receipt.receipt_kind)?;
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_claim(&transaction, claim, "uncertain")?;
        let uncertain_at: String = transaction.query_row(
            "SELECT completed_at FROM outbox WHERE wake_id = ?1 AND state = 'uncertain'",
            [&claim.wake_id],
            |row| row.get(0),
        )?;
        if resolved_at < parse_timestamp("uncertain completion", &uncertain_at)? {
            return Err(WorkLedgerError::Refused(
                "delivery resolution predates the uncertain outcome".to_owned(),
            ));
        }

        let (outbox_state, work_phase, event_kind) = match receipt.receipt_kind {
            DeliveryReceiptKind::Accepted => (
                "acknowledged",
                LifecycleState::AgentOwnedRepair,
                "wake_uncertainty_resolved_accepted",
            ),
            DeliveryReceiptKind::ReconciledNotDelivered => (
                "failed",
                LifecycleState::Actionable,
                "wake_uncertainty_resolved_not_delivered",
            ),
            _ => unreachable!("receipt kind checked above"),
        };
        let now = resolved_at.to_rfc3339();
        let changed = transaction.execute(
            "UPDATE outbox SET state = ?1, receipt_kind = ?2,
                    receipt_digest = ?3, completed_at = ?4, updated_at = ?4
             WHERE wake_id = ?5 AND state = 'uncertain' AND claim_id = ?6
               AND delivery_started_at IS NOT NULL",
            params![
                outbox_state,
                receipt.receipt_kind.as_str(),
                receipt.receipt_integrity,
                now,
                claim.wake_id,
                claim.claim_id,
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "uncertain delivery is no longer resolvable".to_owned(),
            ));
        }
        let transitioned = transaction.execute(
            "UPDATE work_items SET phase = ?1, work_generation = work_generation + 1,
                    updated_at = ?2
             WHERE id = ?3 AND phase = 'dispatching' AND work_generation = ?4
               AND owner_generation = ?5 AND head_sha = ?6",
            params![
                work_phase.as_str(),
                now,
                claim.work_id,
                claim.work_generation,
                claim.owner_generation,
                claim.head_sha,
            ],
        )?;
        if transitioned != 1 {
            return Err(WorkLedgerError::Refused(
                "work changed before uncertain delivery resolution".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &claim.work_id,
            claim.work_generation + 1,
            claim.owner_generation,
            event_kind,
            Some(LifecycleState::Dispatching),
            work_phase,
            &receipt.receipt_integrity,
            &now,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Recover the exact secret-free claim needed to resolve a crash-uncertain wake.
    pub(super) fn recover_uncertain_claim(&self, wake_id: &str) -> WorkLedgerResult<DeliveryClaim> {
        validate_opaque_ref("wake_id", wake_id, "wake")?;
        let connection = self.delivery_connection()?;
        let payload: Vec<u8> = connection
            .query_row(
                "SELECT claim_payload_json FROM outbox
                 WHERE wake_id = ?1 AND state = 'uncertain'",
                [wake_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| WorkLedgerError::Refused("wake is not uncertain".to_owned()))?;
        let claim: DeliveryClaim = serde_json::from_slice(&payload).map_err(|_| {
            WorkLedgerError::Refused("stored delivery claim is malformed".to_owned())
        })?;
        claim.validate_identity()?;
        let matches: bool = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM outbox
               WHERE wake_id = ?1 AND state = 'uncertain' AND claim_id = ?2
                 AND claimant_ref = ?3 AND claim_attempt = ?4
                 AND claim_identity_digest = ?5 AND claimed_at = ?6
                 AND lease_expires_at = ?7 AND delivery_started_at IS NOT NULL
             )",
            params![
                claim.wake_id,
                claim.claim_id,
                claim.claimant_ref,
                claim.claim_attempt,
                claim.identity_digest,
                claim.claimed_at.to_rfc3339(),
                claim.claim_expires_at.to_rfc3339(),
            ],
            |row| row.get(0),
        )?;
        if !matches || claim.wake_id != wake_id {
            return Err(WorkLedgerError::Refused(
                "stored uncertain claim disagrees with its durable identity".to_owned(),
            ));
        }
        Ok(claim)
    }

    /// Persist a definitive local refusal before any external delivery began.
    pub(super) fn fail_unstarted_claim(
        &self,
        claim: &DeliveryClaim,
        failure_digest: &str,
        observed_at: DateTime<Utc>,
    ) -> WorkLedgerResult<()> {
        validate_digest("delivery failure evidence", failure_digest)?;
        claim.validate_identity()?;
        if observed_at < claim.claimed_at || observed_at > claim.claim_expires_at {
            return Err(WorkLedgerError::Refused(
                "definitive delivery failure is outside its claim lease".to_owned(),
            ));
        }
        let receipt = DeliveryReceipt::outcome(
            claim,
            DeliveryReceiptKind::DefinitivePreDeliveryFailure,
            failure_digest,
        )?;
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_claim(&transaction, claim, "claimed")?;
        let now = observed_at.to_rfc3339();
        let changed = transaction.execute(
            "UPDATE outbox SET state = 'failed',
                    receipt_kind = 'definitive_pre_delivery_failure',
                    receipt_digest = ?1, completed_at = ?2, updated_at = ?2
             WHERE wake_id = ?3 AND state = 'claimed' AND claim_id = ?4
               AND delivery_started_at IS NULL",
            params![
                receipt.receipt_integrity,
                now,
                claim.wake_id,
                claim.claim_id
            ],
        )?;
        if changed != 1 {
            return Err(WorkLedgerError::Refused(
                "delivery claim is no longer fail-able before delivery".to_owned(),
            ));
        }
        let transitioned = transaction.execute(
            "UPDATE work_items SET phase = 'actionable',
                    work_generation = work_generation + 1, updated_at = ?1
             WHERE id = ?2 AND phase = 'dispatching' AND work_generation = ?3
               AND owner_generation = ?4 AND head_sha = ?5",
            params![
                now,
                claim.work_id,
                claim.work_generation,
                claim.owner_generation,
                claim.head_sha,
            ],
        )?;
        if transitioned != 1 {
            return Err(WorkLedgerError::Refused(
                "work changed before definitive delivery failure".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &claim.work_id,
            claim.work_generation + 1,
            claim.owner_generation,
            "wake_delivery_failed",
            Some(LifecycleState::Dispatching),
            LifecycleState::Actionable,
            &receipt.receipt_integrity,
            &now,
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Reconcile an expired claim after process or scheduler restart.
    pub(super) fn reconcile_expired_claim(
        &self,
        wake_id: &str,
        observed_at: DateTime<Utc>,
        uncertainty_digest: &str,
    ) -> WorkLedgerResult<ExpiredClaimDisposition> {
        validate_opaque_ref("wake_id", wake_id, "wake")?;
        validate_digest("uncertainty evidence", uncertainty_digest)?;
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(self.writer_parent()?)?;
        let mut connection = self.delivery_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claim = expired_claim(&transaction, wake_id)?;
        if observed_at < claim.expires_at {
            return Err(WorkLedgerError::Refused(
                "delivery claim has not expired".to_owned(),
            ));
        }
        let now = observed_at.to_rfc3339();
        let disposition = if claim.state == "claimed" && claim.started_at.is_none() {
            requeue_expired_unstarted(&transaction, wake_id, &claim, &now)?;
            ExpiredClaimDisposition::RequeuedUnstarted
        } else {
            let receipt_digest = receipt_integrity(
                DeliveryReceiptKind::Uncertain,
                wake_id,
                &claim.claim_id,
                &claim.claim_identity,
                None,
                uncertainty_digest,
            )?;
            mark_expired_started_uncertain(&transaction, wake_id, &claim, &receipt_digest, &now)?;
            ExpiredClaimDisposition::MarkedUncertain
        };
        transaction.commit()?;
        Ok(disposition)
    }

    fn writer_parent(&self) -> WorkLedgerResult<&std::path::Path> {
        self.path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))
    }

    fn delivery_connection(&self) -> WorkLedgerResult<rusqlite::Connection> {
        let connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        Ok(connection)
    }
}

fn pending_wake(transaction: &Transaction<'_>, wake_id: &str) -> WorkLedgerResult<PendingWake> {
    let pending = transaction
        .query_row(
            "SELECT wake_id, work_item_id, work_generation, owner_generation,
                    route_ref, payload_digest, claim_attempt
             FROM outbox WHERE wake_id = ?1 AND state = 'pending'",
            [wake_id],
            |row| {
                Ok(PendingWake {
                    wake_id: row.get(0)?,
                    work_id: row.get(1)?,
                    work_generation: row.get(2)?,
                    owner_generation: row.get(3)?,
                    route_ref: row.get(4)?,
                    payload_digest: row.get(5)?,
                    claim_attempt: row.get(6)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| WorkLedgerError::Refused("wake is not pending".to_owned()))?;
    Ok(pending)
}

fn expired_claim(
    transaction: &Transaction<'_>,
    wake_id: &str,
) -> WorkLedgerResult<ExpiredClaimRecord> {
    transaction
        .query_row(
            "SELECT claim_id, claim_identity_digest, lease_expires_at,
                    state, delivery_started_at, work_item_id,
                    work_generation, owner_generation
             FROM outbox WHERE wake_id = ?1
               AND state IN ('claimed', 'delivery_started')",
            [wake_id],
            |row| {
                let expires_at: String = row.get(2)?;
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    expires_at,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| WorkLedgerError::Refused("wake has no active claim to reconcile".to_owned()))
        .and_then(
            |(
                claim_id,
                claim_identity,
                expires_at,
                state,
                started_at,
                work_id,
                work_generation,
                owner_generation,
            )| {
                Ok(ExpiredClaimRecord {
                    claim_id,
                    claim_identity,
                    expires_at: parse_timestamp("claim expiry", &expires_at)?,
                    state,
                    started_at,
                    work_id,
                    work_generation,
                    owner_generation,
                })
            },
        )
}

fn requeue_expired_unstarted(
    transaction: &Transaction<'_>,
    wake_id: &str,
    claim: &ExpiredClaimRecord,
    observed_at: &str,
) -> WorkLedgerResult<()> {
    let changed = transaction.execute(
        "UPDATE outbox SET state = 'pending', claim_id = NULL,
                claimant_ref = NULL, claimed_at = NULL,
                claim_identity_digest = NULL, lease_expires_at = NULL,
                claim_payload_json = NULL, delivery_started_at = NULL, updated_at = ?1
         WHERE wake_id = ?2 AND state = 'claimed' AND claim_id = ?3
           AND delivery_started_at IS NULL",
        params![observed_at, wake_id, claim.claim_id],
    )?;
    if changed != 1 {
        return Err(WorkLedgerError::Refused(
            "unstarted delivery claim changed during reconciliation".to_owned(),
        ));
    }
    record_event(
        transaction,
        &claim.work_id,
        claim.work_generation,
        claim.owner_generation,
        "wake_claim_requeued",
        Some(LifecycleState::Dispatching),
        LifecycleState::Dispatching,
        &claim.claim_identity,
        observed_at,
    )
}

fn mark_expired_started_uncertain(
    transaction: &Transaction<'_>,
    wake_id: &str,
    claim: &ExpiredClaimRecord,
    receipt_digest: &str,
    observed_at: &str,
) -> WorkLedgerResult<()> {
    update_uncertain(
        transaction,
        wake_id,
        &claim.claim_id,
        receipt_digest,
        observed_at,
    )?;
    record_event(
        transaction,
        &claim.work_id,
        claim.work_generation,
        claim.owner_generation,
        "wake_delivery_uncertain",
        Some(LifecycleState::Dispatching),
        LifecycleState::Dispatching,
        receipt_digest,
        observed_at,
    )
}

fn stored_wake_for_claim(claim: &DeliveryClaim) -> PendingWake {
    PendingWake {
        wake_id: claim.wake_id.clone(),
        work_id: claim.work_id.clone(),
        work_generation: claim.work_generation,
        owner_generation: claim.owner_generation,
        route_ref: claim.route_ref.clone(),
        payload_digest: claim.payload_digest.clone(),
        claim_attempt: claim.claim_attempt,
    }
}

fn validated_delivery_route(
    transaction: &Transaction<'_>,
    wake: &PendingWake,
) -> WorkLedgerResult<(String, DeliveryRouteIdentity)> {
    let route_generation = wake.work_generation.checked_sub(1).ok_or_else(|| {
        WorkLedgerError::Refused("wake generation cannot precede its route".to_owned())
    })?;
    if !validated_route_exists(
        transaction,
        &wake.route_ref,
        &wake.work_id,
        route_generation,
        wake.owner_generation,
    )? {
        return Err(WorkLedgerError::Refused(
            "wake route is stale or unavailable".to_owned(),
        ));
    }
    let (head, owner) = current_dispatch_truth(transaction, wake)?;
    let route = stored_route(transaction, &wake.route_ref)?;
    if route.head != head
        || owner.as_deref() != Some(route.owner.as_str())
        || route.owner_generation != wake.owner_generation
    {
        return Err(WorkLedgerError::Refused(
            "wake route no longer matches current head or owner".to_owned(),
        ));
    }
    let identity = route_identity(route)?;
    Ok((head, identity))
}

fn current_dispatch_truth(
    transaction: &Transaction<'_>,
    wake: &PendingWake,
) -> WorkLedgerResult<(String, Option<String>)> {
    let work: (Option<String>, String, u64, u64, Option<String>) = transaction.query_row(
        "SELECT head_sha, phase, work_generation, owner_generation, owner_id
         FROM work_items WHERE id = ?1",
        [&wake.work_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        },
    )?;
    let (head, phase, generation, owner_generation, owner_id) = work;
    let head = head
        .ok_or_else(|| WorkLedgerError::Refused("dispatching work has no exact head".to_owned()))?;
    if phase != LifecycleState::Dispatching.as_str()
        || generation != wake.work_generation
        || owner_generation != wake.owner_generation
    {
        return Err(WorkLedgerError::Refused(
            "work changed before wake delivery".to_owned(),
        ));
    }
    Ok((head, owner_id))
}

fn stored_route(
    transaction: &Transaction<'_>,
    route_ref: &str,
) -> WorkLedgerResult<StoredRouteRow> {
    Ok(transaction.query_row(
        "SELECT head_sha, owner_ref, owner_generation, revision, payload_json,
                integrity_hash, terminal_kind, agent_kind, provider_kind
         FROM route_records WHERE route_ref = ?1",
        [route_ref],
        |row| {
            Ok(StoredRouteRow {
                head: row.get(0)?,
                owner: row.get(1)?,
                owner_generation: row.get(2)?,
                revision: row.get(3)?,
                payload: row.get(4)?,
                integrity: row.get(5)?,
                terminal_kind: row.get(6)?,
                agent_kind: row.get(7)?,
                provider_kind: row.get(8)?,
            })
        },
    )?)
}

fn route_identity(route: StoredRouteRow) -> WorkLedgerResult<DeliveryRouteIdentity> {
    let provenance: RouteProvenanceRecord = serde_json::from_slice(&route.payload)
        .map_err(|_| WorkLedgerError::Refused("stored route payload is malformed".to_owned()))?;
    provenance
        .validate()
        .map_err(|_| WorkLedgerError::Refused("stored route provenance is invalid".to_owned()))?;
    let session = native_session(&provenance);
    Ok(DeliveryRouteIdentity {
        terminal_kind: route.terminal_kind,
        terminal: provenance.terminal.route.clone(),
        agent_kind: route.agent_kind,
        agent: provenance.agent.clone(),
        provider_kind: route.provider_kind,
        provider: provenance.provider.route.clone(),
        route_revision: route.revision,
        route_integrity: route.integrity,
        native_session_ref: session.native_session_ref.as_str().to_owned(),
        native_resume_ref: session.native_resume_ref.as_str().to_owned(),
        account_ref: session.account_ref.as_str().to_owned(),
        model_ref: session.model_ref.as_str().to_owned(),
        wrapper_ref: session.wrapper_ref.as_str().to_owned(),
        session_headers_ref: session.session_headers_ref.as_str().to_owned(),
        session_headers_sha256: session.session_headers_sha256.as_str().to_owned(),
        launch_profile_ref: provenance.launch_profile.profile_ref.as_str().to_owned(),
        launch_generation: provenance.launch_profile.generation,
        launch_revision: provenance.launch_profile.revision,
        executable_sha256: provenance
            .launch_profile
            .executable_sha256
            .as_str()
            .to_owned(),
        configuration_sha256: provenance
            .launch_profile
            .configuration_sha256
            .as_str()
            .to_owned(),
    })
}

fn native_session(provenance: &RouteProvenanceRecord) -> &NativeSessionRoute {
    match &provenance.agent.route {
        AgentRoute::Codex { session }
        | AgentRoute::Claude { session }
        | AgentRoute::Named { session, .. } => session,
    }
}

fn verify_claim(
    transaction: &Transaction<'_>,
    claim: &DeliveryClaim,
    expected_state: &str,
) -> WorkLedgerResult<()> {
    type StoredClaim = (String, String, u64, String, String, String, Option<String>);
    let stored: Option<StoredClaim> = transaction
        .query_row(
            "SELECT claim_id, claimant_ref, claim_attempt, claim_identity_digest,
                    claimed_at, lease_expires_at, delivery_started_at
             FROM outbox WHERE wake_id = ?1 AND state = ?2",
            params![claim.wake_id, expected_state],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((claim_id, claimant, attempt, identity, claimed_at, expires_at, started_at)) = stored
    else {
        return Err(WorkLedgerError::Refused(
            "delivery claim is not active".to_owned(),
        ));
    };
    if claim_id != claim.claim_id
        || claimant != claim.claimant_ref
        || attempt != claim.claim_attempt
        || identity != claim.identity_digest
        || parse_timestamp("claimed at", &claimed_at)? != claim.claimed_at
        || parse_timestamp("claim expiry", &expires_at)? != claim.claim_expires_at
        || matches!(expected_state, "delivery_started" | "uncertain") != started_at.is_some()
    {
        return Err(WorkLedgerError::Refused(
            "stored delivery claim disagrees with its token".to_owned(),
        ));
    }
    Ok(())
}

fn verify_started(
    transaction: &Transaction<'_>,
    started: &StartedDelivery,
) -> WorkLedgerResult<()> {
    verify_claim(transaction, &started.claim, "delivery_started")?;
    let stored: String = transaction.query_row(
        "SELECT delivery_started_at FROM outbox WHERE wake_id = ?1",
        [&started.claim.wake_id],
        |row| row.get(0),
    )?;
    if parse_timestamp("delivery started at", &stored)? != started.started_at {
        return Err(WorkLedgerError::Refused(
            "stored delivery start disagrees with its token".to_owned(),
        ));
    }
    Ok(())
}

fn update_uncertain(
    transaction: &Transaction<'_>,
    wake_id: &str,
    claim_id: &str,
    uncertainty_digest: &str,
    observed_at: &str,
) -> WorkLedgerResult<()> {
    let changed = transaction.execute(
        "UPDATE outbox SET state = 'uncertain', receipt_kind = 'uncertain',
                receipt_digest = ?1, completed_at = ?2, updated_at = ?2
         WHERE wake_id = ?3 AND state = 'delivery_started' AND claim_id = ?4
           AND delivery_started_at IS NOT NULL",
        params![uncertainty_digest, observed_at, wake_id, claim_id],
    )?;
    if changed != 1 {
        return Err(WorkLedgerError::Refused(
            "started delivery claim is no longer uncertain-able".to_owned(),
        ));
    }
    Ok(())
}

fn receipt_integrity(
    receipt_kind: DeliveryReceiptKind,
    wake_id: &str,
    claim_id: &str,
    delivery_identity_digest: &str,
    observed_native_session_ref: Option<&str>,
    transport_evidence_digest: &str,
) -> WorkLedgerResult<String> {
    validate_opaque_ref("wake_id", wake_id, "wake")?;
    validate_opaque_ref("claim_id", claim_id, "claim")?;
    validate_digest("delivery identity", delivery_identity_digest)?;
    validate_digest("transport evidence", transport_evidence_digest)?;
    let receipt = DeliveryReceipt {
        schema_version: DELIVERY_SCHEMA_VERSION,
        wake_id: wake_id.to_owned(),
        claim_id: claim_id.to_owned(),
        delivery_identity_digest: delivery_identity_digest.to_owned(),
        receipt_kind,
        observed_native_session_ref: observed_native_session_ref.map(str::to_owned),
        transport_evidence_digest: transport_evidence_digest.to_owned(),
        receipt_integrity: String::new(),
    };
    receipt.recompute_integrity()
}

fn parse_timestamp(label: &str, value: &str) -> WorkLedgerResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| WorkLedgerError::Refused(format!("invalid {label} timestamp")))
}
