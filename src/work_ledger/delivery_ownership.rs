//! Receipt-fenced transfer between provider delivery and agent ownership.

use serde::{Deserialize, Serialize};

use super::dispatch::{StoredProviderRequest, StoredResumeExpectation};
use super::lifecycle::record_event;
use super::{
    LifecycleState, OptionalExtension, ProtectedObjectKind, TransactionBehavior, Utc, WorkLedger,
    WorkLedgerError, WorkLedgerResult, configure_durable, digest, opaque_ref, params,
    validate_digest, verify_integrity, verify_supported_schema,
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentContextReceipt {
    pub(crate) schema_version: u32,
    pub(crate) wake_id: String,
    pub(crate) work_item_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) delivery_id: String,
    pub(crate) idempotency_key: String,
    pub(crate) provider_receipt_digest: String,
    pub(crate) workstream_handle: String,
    pub(crate) context_url: Option<String>,
    pub(crate) plan_sha256: String,
    pub(crate) root_revision: u64,
    pub(crate) issue_revision: u64,
    pub(crate) material_event_revision: u64,
    pub(crate) projection_revision: u64,
    pub(crate) checkpoint_id: String,
    pub(crate) checkpoint_generation: u64,
    pub(crate) checkpoint_digest: String,
    pub(crate) repository: String,
    pub(crate) head_sha: String,
    pub(crate) resume_context_digest: String,
    pub(crate) success_continuation_digest: String,
    pub(crate) failure_continuation_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentReturnExpectation {
    pub(crate) schema_version: u32,
    pub(crate) work_item_id: String,
    pub(crate) ownership_id: String,
    pub(crate) delivery_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) context_receipt_digest: String,
    pub(crate) checkpoint_id: String,
    pub(crate) checkpoint_generation: u64,
    pub(crate) checkpoint_digest: String,
    pub(crate) repository: String,
    pub(crate) head_sha: String,
    pub(crate) evidence_digest: String,
    pub(crate) remote_acknowledgement_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentReturnReceipt {
    pub(crate) schema_version: u32,
    pub(crate) work_item_id: String,
    pub(crate) ownership_id: String,
    pub(crate) delivery_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) context_receipt_digest: String,
    pub(crate) checkpoint_id: String,
    pub(crate) checkpoint_generation: u64,
    pub(crate) checkpoint_digest: String,
    pub(crate) repository: String,
    pub(crate) head_sha: String,
    pub(crate) evidence_digest: String,
    pub(crate) remote_acknowledgement_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AgentOwnershipReceipt {
    pub(crate) ownership_id: String,
    pub(crate) receipt_object_ref: String,
    pub(crate) receipt_digest: String,
}

#[derive(Clone, Debug)]
struct DeliveredAuthority {
    wake_id: String,
    work_item_id: String,
    work_generation: u64,
    owner_generation: u64,
    delivery_id: String,
    attempt: u64,
    request_object_ref: String,
    profile_object_ref: String,
    idempotency_key: String,
    provider_receipt_digest: String,
}

impl WorkLedger {
    pub(crate) fn acknowledge_agent_context(
        &self,
        wake_id: &str,
        receipt_bytes: &[u8],
    ) -> WorkLedgerResult<AgentOwnershipReceipt> {
        let authority = self.delivered_authority(wake_id)?;
        let (_, request_bytes) = self.open_protected_object(&authority.request_object_ref)?;
        let request: StoredProviderRequest =
            serde_json::from_slice(&request_bytes).map_err(|_| {
                WorkLedgerError::Refused("provider request authority is malformed".to_owned())
            })?;
        let receipt: AgentContextReceipt = serde_json::from_slice(receipt_bytes).map_err(|_| {
            WorkLedgerError::Refused("agent context receipt is malformed".to_owned())
        })?;
        validate_context_receipt(&authority, &request.resume, &receipt)?;
        let receipt_digest = digest(receipt_bytes);
        let receipt_object = self.put_protected_object(
            &authority.work_item_id,
            ProtectedObjectKind::AgentReceipt,
            None,
            &receipt_digest,
            receipt_bytes,
        )?;
        let ownership_id = opaque_ref(
            "ao",
            &format!(
                "shipyard-agent-ownership-v1\n{}\n{}\n{}",
                authority.delivery_id, authority.work_generation, receipt_digest
            ),
        );

        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String, String)> = transaction
            .query_row(
                "SELECT ownership_id, context_receipt_object_ref, context_receipt_digest
                 FROM agent_ownership WHERE delivery_id = ?1",
                [&authority.delivery_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some(existing) = existing {
            if existing
                == (
                    ownership_id.clone(),
                    receipt_object.object_ref.clone(),
                    receipt_digest.clone(),
                )
            {
                return Ok(AgentOwnershipReceipt {
                    ownership_id,
                    receipt_object_ref: receipt_object.object_ref,
                    receipt_digest,
                });
            }
            return Err(WorkLedgerError::Refused(
                "provider delivery is already bound to different agent ownership".to_owned(),
            ));
        }
        verify_delivered_authority(&transaction, wake_id, &authority)?;
        let now = Utc::now().to_rfc3339();
        transaction.execute(
            "INSERT INTO agent_ownership
             (ownership_id, work_item_id, work_generation, owner_generation,
              delivery_id, launch_profile_object_ref, context_receipt_object_ref,
              state, context_receipt_digest, created_at, updated_at, acknowledged_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'acknowledged', ?8, ?9, ?9, ?9)",
            params![
                ownership_id,
                authority.work_item_id,
                authority.work_generation,
                authority.owner_generation,
                authority.delivery_id,
                authority.profile_object_ref,
                receipt_object.object_ref,
                receipt_digest,
                now,
            ],
        )?;
        let attempt_changed = transaction.execute(
            "UPDATE wake_attempts SET state = 'acknowledged'
             WHERE wake_id = ?1 AND attempt = ?2 AND state = 'delivered'",
            params![wake_id, authority.attempt],
        )?;
        let wake_changed = transaction.execute(
            "UPDATE outbox SET state = 'acknowledged', acknowledged_at = ?1, updated_at = ?1
             WHERE wake_id = ?2 AND state = 'delivered' AND provider_delivery_id = ?3",
            params![now, wake_id, authority.delivery_id],
        )?;
        let work_changed = transaction.execute(
            "UPDATE work_items SET phase = 'agent_owned_repair',
                    work_generation = work_generation + 1, updated_at = ?1
             WHERE id = ?2 AND phase = 'dispatching'
               AND work_generation = ?3 AND owner_generation = ?4",
            params![
                now,
                authority.work_item_id,
                authority.work_generation,
                authority.owner_generation,
            ],
        )?;
        if attempt_changed != 1 || wake_changed != 1 || work_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "delivery changed during agent context acknowledgement".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &authority.work_item_id,
            authority.work_generation + 1,
            authority.owner_generation,
            "agent_context_acknowledged",
            Some(LifecycleState::Dispatching),
            LifecycleState::AgentOwnedRepair,
            &receipt_digest,
            &now,
        )?;
        transaction.commit()?;
        Ok(AgentOwnershipReceipt {
            ownership_id,
            receipt_object_ref: receipt_object.object_ref,
            receipt_digest,
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn return_agent_ownership(
        &self,
        ownership_id: &str,
        expected_delivery_id: &str,
        expected_work_generation: u64,
        expected: &AgentReturnExpectation,
        receipt_bytes: &[u8],
    ) -> WorkLedgerResult<AgentOwnershipReceipt> {
        validate_return_expectation(expected)?;
        let receipt: AgentReturnReceipt = serde_json::from_slice(receipt_bytes).map_err(|_| {
            WorkLedgerError::Refused("agent return receipt is malformed".to_owned())
        })?;
        if receipt.schema_version != expected.schema_version
            || receipt.work_item_id != expected.work_item_id
            || receipt.ownership_id != expected.ownership_id
            || receipt.delivery_id != expected.delivery_id
            || receipt.work_generation != expected.work_generation
            || receipt.owner_generation != expected.owner_generation
            || receipt.context_receipt_digest != expected.context_receipt_digest
            || receipt.checkpoint_id != expected.checkpoint_id
            || receipt.checkpoint_generation != expected.checkpoint_generation
            || receipt.checkpoint_digest != expected.checkpoint_digest
            || receipt.repository != expected.repository
            || receipt.head_sha != expected.head_sha
            || receipt.evidence_digest != expected.evidence_digest
            || receipt.remote_acknowledgement_digest != expected.remote_acknowledgement_digest
        {
            return Err(WorkLedgerError::Refused(
                "agent return receipt does not match reviewed final authority".to_owned(),
            ));
        }
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let authority: (String, String, u64, u64, String, String, u64, String) = connection
            .query_row(
                "SELECT ownership.work_item_id, ownership.delivery_id,
                        ownership.work_generation, ownership.owner_generation,
                        ownership.state, delivery.request_object_ref, delivery.attempt,
                        ownership.context_receipt_digest
                 FROM agent_ownership ownership
                 JOIN provider_deliveries delivery ON delivery.delivery_id = ownership.delivery_id
                 WHERE ownership.ownership_id = ?1",
                [ownership_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| WorkLedgerError::Refused("agent ownership is missing".to_owned()))?;
        if authority.0 != expected.work_item_id
            || ownership_id != expected.ownership_id
            || authority.1 != expected_delivery_id
            || authority.1 != expected.delivery_id
            || authority.2.checked_add(1) != Some(expected_work_generation)
            || expected_work_generation != expected.work_generation
            || authority.3 != expected.owner_generation
            || authority.7 != expected.context_receipt_digest
        {
            return Err(WorkLedgerError::Refused(
                "agent return ownership CAS does not match".to_owned(),
            ));
        }
        let (_, request_bytes) = self.open_protected_object(&authority.5)?;
        let request: StoredProviderRequest =
            serde_json::from_slice(&request_bytes).map_err(|_| {
                WorkLedgerError::Refused("provider request authority is malformed".to_owned())
            })?;
        if expected.checkpoint_generation <= request.resume.checkpoint_generation
            || expected.checkpoint_digest == request.resume.checkpoint_digest
            || expected.repository != request.resume.repository
        {
            return Err(WorkLedgerError::Refused(
                "agent return does not prove a newer checkpoint on the expected repository"
                    .to_owned(),
            ));
        }
        let receipt_digest = digest(receipt_bytes);
        let receipt_object = self.put_protected_object(
            &authority.0,
            ProtectedObjectKind::AgentReceipt,
            None,
            &receipt_digest,
            receipt_bytes,
        )?;
        let parent = self
            .path
            .parent()
            .ok_or_else(|| WorkLedgerError::Refused("database has no parent".to_owned()))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let mut connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: (String, String, u64, u64) = transaction.query_row(
            "SELECT ownership.state, work.phase, work.work_generation, work.owner_generation
             FROM agent_ownership ownership
             JOIN work_items work ON work.id = ownership.work_item_id
             WHERE ownership.ownership_id = ?1 AND ownership.delivery_id = ?2",
            params![ownership_id, expected_delivery_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        if current.0 == "returned" && current.1 == LifecycleState::Returned.as_str() {
            let exact_event: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM events
                 WHERE work_item_id = ?1 AND kind = 'agent_ownership_returned'
                   AND payload_digest = ?2)",
                params![authority.0, receipt_digest],
                |row| row.get(0),
            )?;
            if exact_event {
                return Ok(AgentOwnershipReceipt {
                    ownership_id: ownership_id.to_owned(),
                    receipt_object_ref: receipt_object.object_ref,
                    receipt_digest,
                });
            }
        }
        if current
            != (
                "acknowledged".to_owned(),
                LifecycleState::AgentOwnedRepair.as_str().to_owned(),
                expected_work_generation,
                authority.3,
            )
        {
            return Err(WorkLedgerError::Refused(
                "agent ownership changed before return".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let ownership_changed = transaction.execute(
            "UPDATE agent_ownership SET state = 'returned', returned_at = ?1, updated_at = ?1
             WHERE ownership_id = ?2 AND delivery_id = ?3 AND state = 'acknowledged'",
            params![now, ownership_id, expected_delivery_id],
        )?;
        let work_changed = transaction.execute(
            "UPDATE work_items SET phase = 'returned', work_generation = work_generation + 1,
                    updated_at = ?1
             WHERE id = ?2 AND phase = 'agent_owned_repair'
               AND work_generation = ?3 AND owner_generation = ?4",
            params![now, authority.0, expected_work_generation, authority.3],
        )?;
        if ownership_changed != 1 || work_changed != 1 {
            return Err(WorkLedgerError::Refused(
                "agent ownership changed during return".to_owned(),
            ));
        }
        record_event(
            &transaction,
            &authority.0,
            expected_work_generation + 1,
            authority.3,
            "agent_ownership_returned",
            Some(LifecycleState::AgentOwnedRepair),
            LifecycleState::Returned,
            &receipt_digest,
            &now,
        )?;
        transaction.commit()?;
        Ok(AgentOwnershipReceipt {
            ownership_id: ownership_id.to_owned(),
            receipt_object_ref: receipt_object.object_ref,
            receipt_digest,
        })
    }

    fn delivered_authority(&self, wake_id: &str) -> WorkLedgerResult<DeliveredAuthority> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        connection
            .query_row(
                "SELECT wake.work_item_id, wake.work_generation, wake.owner_generation,
                        delivery.delivery_id, delivery.attempt, delivery.request_object_ref,
                        profile.object_ref, delivery.idempotency_key, receipt.content_digest
                 FROM outbox wake
                 JOIN provider_deliveries delivery
                   ON delivery.delivery_id = wake.provider_delivery_id
                 JOIN protected_objects profile
                   ON profile.work_item_id = wake.work_item_id
                  AND profile.profile_ref = wake.profile_ref
                 JOIN protected_objects receipt
                   ON receipt.object_ref = delivery.receipt_object_ref
                 JOIN work_items work ON work.id = wake.work_item_id
                 WHERE wake.wake_id = ?1 AND wake.state IN ('delivered', 'acknowledged')
                   AND delivery.state = 'delivered'
                   AND ((wake.state = 'delivered' AND work.phase = 'dispatching'
                         AND work.work_generation = wake.work_generation)
                        OR (wake.state = 'acknowledged' AND work.phase = 'agent_owned_repair'
                            AND work.work_generation = wake.work_generation + 1)
                        OR (wake.state = 'acknowledged' AND work.phase = 'returned'
                            AND work.work_generation = wake.work_generation + 2))
                   AND work.owner_generation = wake.owner_generation",
                [wake_id],
                |row| {
                    Ok(DeliveredAuthority {
                        wake_id: wake_id.to_owned(),
                        work_item_id: row.get(0)?,
                        work_generation: row.get(1)?,
                        owner_generation: row.get(2)?,
                        delivery_id: row.get(3)?,
                        attempt: row.get(4)?,
                        request_object_ref: row.get(5)?,
                        profile_object_ref: row.get(6)?,
                        idempotency_key: row.get(7)?,
                        provider_receipt_digest: row.get(8)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| WorkLedgerError::Refused("wake is not exactly delivered".to_owned()))
    }
}

fn validate_context_receipt(
    authority: &DeliveredAuthority,
    expected: &StoredResumeExpectation,
    actual: &AgentContextReceipt,
) -> WorkLedgerResult<()> {
    let exact = actual.schema_version == 1
        && actual.wake_id == authority.wake_id
        && actual.work_item_id == authority.work_item_id
        && actual.work_generation == authority.work_generation
        && actual.owner_generation == authority.owner_generation
        && actual.delivery_id == authority.delivery_id
        && actual.idempotency_key == authority.idempotency_key
        && actual.provider_receipt_digest == authority.provider_receipt_digest
        && actual.workstream_handle == expected.workstream_handle
        && actual.context_url == expected.context_url
        && actual.plan_sha256 == expected.plan_sha256
        && actual.root_revision == expected.root_revision
        && actual.issue_revision == expected.issue_revision
        && actual.material_event_revision == expected.material_event_revision
        && actual.projection_revision == expected.projection_revision
        && actual.checkpoint_id == expected.checkpoint_id
        && actual.checkpoint_generation == expected.checkpoint_generation
        && actual.checkpoint_digest == expected.checkpoint_digest
        && actual.repository == expected.repository
        && actual.head_sha == expected.head_sha
        && actual.resume_context_digest == expected.expected_resume_context_digest
        && actual.success_continuation_digest == expected.success_continuation_digest
        && actual.failure_continuation_digest == expected.failure_continuation_digest;
    if !exact {
        return Err(WorkLedgerError::Refused(
            "agent context receipt does not match immutable resume authority".to_owned(),
        ));
    }
    Ok(())
}

fn validate_return_expectation(expected: &AgentReturnExpectation) -> WorkLedgerResult<()> {
    validate_digest(
        "return context receipt digest",
        &expected.context_receipt_digest,
    )?;
    validate_digest("return checkpoint digest", &expected.checkpoint_digest)?;
    validate_digest("return evidence digest", &expected.evidence_digest)?;
    validate_digest(
        "return remote acknowledgement digest",
        &expected.remote_acknowledgement_digest,
    )?;
    if expected.schema_version != 1
        || expected.work_item_id.is_empty()
        || expected.ownership_id.is_empty()
        || expected.delivery_id.is_empty()
        || expected.work_generation == 0
        || expected.owner_generation == 0
        || expected.checkpoint_id.is_empty()
        || expected.checkpoint_generation == 0
        || expected.repository.is_empty()
        || expected.head_sha.len() != 40
        || !expected
            .head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkLedgerError::Refused(
            "agent return expectation is malformed".to_owned(),
        ));
    }
    Ok(())
}

fn verify_delivered_authority(
    transaction: &rusqlite::Transaction<'_>,
    wake_id: &str,
    expected: &DeliveredAuthority,
) -> WorkLedgerResult<()> {
    let exact: bool = transaction.query_row(
        "SELECT EXISTS(
           SELECT 1 FROM outbox wake
           JOIN provider_deliveries delivery ON delivery.delivery_id = wake.provider_delivery_id
           JOIN work_items work ON work.id = wake.work_item_id
           WHERE wake.wake_id = ?1 AND wake.work_item_id = ?2
             AND wake.work_generation = ?3 AND wake.owner_generation = ?4
             AND wake.state = 'delivered' AND delivery.delivery_id = ?5
             AND delivery.attempt = ?6 AND delivery.request_object_ref = ?7
             AND delivery.state = 'delivered' AND work.phase = 'dispatching'
             AND work.work_generation = ?3 AND work.owner_generation = ?4)",
        params![
            wake_id,
            expected.work_item_id,
            expected.work_generation,
            expected.owner_generation,
            expected.delivery_id,
            expected.attempt,
            expected.request_object_ref,
        ],
        |row| row.get(0),
    )?;
    if !exact {
        return Err(WorkLedgerError::Refused(
            "delivered authority changed before acknowledgement".to_owned(),
        ));
    }
    Ok(())
}
