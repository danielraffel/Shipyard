//! Protected context acknowledgement and receipt-fenced ownership return.
//!
//! Provider acceptance is not proof that a resumed agent reconstructed its
//! durable context. These APIs keep both acknowledgements private, immutable,
//! and bound to the exact delivery/work generations without adding schema.

use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

use super::protected_objects::ProtectedObjectKind;
use super::{LifecycleState, WorkLedger, WorkLedgerError, WorkLedgerResult, digest, opaque_ref};

const RECEIPT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AgentContextChallenge {
    pub(crate) ownership_id: String,
    pub(crate) wake_id: String,
    pub(crate) claim_id: String,
    pub(crate) work_id: String,
    pub(crate) head_sha: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) route_ref: String,
    pub(crate) delivery_identity_digest: String,
    pub(crate) expected_resume_context_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentContextReceipt {
    pub(crate) schema_version: u32,
    pub(crate) ownership_id: String,
    pub(crate) wake_id: String,
    pub(crate) claim_id: String,
    pub(crate) head_sha: String,
    pub(crate) delivery_identity_digest: String,
    pub(crate) reconstructed_context_digest: String,
    pub(crate) agent_evidence_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct AgentOwnership {
    pub(crate) challenge: AgentContextChallenge,
    pub(crate) context_receipt_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentReturnReceipt {
    pub(crate) schema_version: u32,
    pub(crate) ownership_id: String,
    pub(crate) context_receipt_digest: String,
    pub(crate) next_checkpoint_digest: String,
    pub(crate) evidence_digest: String,
    pub(crate) remote_acknowledgement_digest: String,
}

impl WorkLedger {
    pub(crate) fn agent_context_challenge(
        &self,
        wake_id: &str,
    ) -> WorkLedgerResult<AgentContextChallenge> {
        let claim = self.recover_acknowledged_claim(wake_id)?;
        let expected_resume_context_digest = self.expected_resume_context_digest(&claim)?;
        let ownership_id = opaque_ref(
            "ownership",
            &format!(
                "{}\n{}\n{}\n{}",
                claim.ledger_incarnation_ref, claim.wake_id, claim.claim_id, claim.identity_digest
            ),
        );
        Ok(AgentContextChallenge {
            ownership_id,
            wake_id: claim.wake_id,
            claim_id: claim.claim_id,
            work_id: claim.work_id,
            head_sha: claim.head_sha,
            work_generation: claim.work_generation,
            owner_generation: claim.owner_generation,
            route_ref: claim.route_ref,
            delivery_identity_digest: claim.identity_digest,
            expected_resume_context_digest,
        })
    }

    pub(crate) fn acknowledge_agent_context(
        &self,
        challenge: &AgentContextChallenge,
        receipt: &AgentContextReceipt,
    ) -> WorkLedgerResult<AgentOwnership> {
        validate_context_receipt(challenge, receipt)?;
        let bytes = serde_json::to_vec(receipt).map_err(|_| {
            WorkLedgerError::Refused("agent context receipt cannot be serialized".to_owned())
        })?;
        let receipt_digest = digest(&bytes);
        let claim = self.recover_acknowledged_claim(&challenge.wake_id)?;
        if challenge != &self.agent_context_challenge(&challenge.wake_id)?
            || claim.work_id != challenge.work_id
        {
            return Err(WorkLedgerError::Refused(
                "agent context challenge is stale".to_owned(),
            ));
        }
        self.put_protected_object_with_transaction(
            &challenge.work_id,
            ProtectedObjectKind::AgentReceipt,
            None,
            &receipt_digest,
            &bytes,
            |transaction, _| require_exact_agent_owned(transaction, challenge),
        )?;
        Ok(AgentOwnership {
            challenge: challenge.clone(),
            context_receipt_digest: receipt_digest,
        })
    }

    pub(crate) fn return_agent_ownership(
        &self,
        ownership: &AgentOwnership,
        receipt: &AgentReturnReceipt,
    ) -> WorkLedgerResult<String> {
        validate_return_receipt(ownership, receipt)?;
        let bytes = serde_json::to_vec(receipt).map_err(|_| {
            WorkLedgerError::Refused("agent return receipt cannot be serialized".to_owned())
        })?;
        let receipt_digest = digest(&bytes);
        self.put_protected_object_with_transaction(
            &ownership.challenge.work_id,
            ProtectedObjectKind::AgentReceipt,
            None,
            &receipt_digest,
            &bytes,
            |transaction, now| {
                require_exact_agent_owned(transaction, &ownership.challenge)?;
                let next_generation = ownership.challenge.work_generation + 2;
                let changed = transaction.execute(
                    "UPDATE work_items SET phase = 'returned',
                            work_generation = work_generation + 1, updated_at = ?1
                     WHERE id = ?2 AND head_sha = ?3 AND phase = 'agent_owned_repair'
                       AND work_generation = ?4 AND owner_generation = ?5",
                    params![
                        now,
                        ownership.challenge.work_id,
                        ownership.challenge.head_sha,
                        ownership.challenge.work_generation + 1,
                        ownership.challenge.owner_generation,
                    ],
                )?;
                if changed != 1 {
                    return Err(WorkLedgerError::Refused(
                        "agent ownership authority changed before return".to_owned(),
                    ));
                }
                super::lifecycle::record_event(
                    transaction,
                    &self.ledger_incarnation_ref,
                    None,
                    &ownership.challenge.work_id,
                    next_generation,
                    ownership.challenge.owner_generation,
                    "agent_ownership_returned",
                    Some(LifecycleState::AgentOwnedRepair),
                    LifecycleState::Returned,
                    &receipt_digest,
                    now,
                )
            },
        )?;
        Ok(receipt_digest)
    }
}

fn require_exact_agent_owned(
    transaction: &Transaction<'_>,
    challenge: &AgentContextChallenge,
) -> WorkLedgerResult<()> {
    let exact_state: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items
         WHERE id = ?1 AND head_sha = ?2 AND phase = 'agent_owned_repair'
           AND work_generation = ?3 AND owner_generation = ?4)",
        params![
            challenge.work_id,
            challenge.head_sha,
            challenge.work_generation + 1,
            challenge.owner_generation,
        ],
        |row| row.get(0),
    )?;
    if !exact_state {
        return Err(WorkLedgerError::Refused(
            "agent ownership authority changed".to_owned(),
        ));
    }
    Ok(())
}

fn validate_context_receipt(
    challenge: &AgentContextChallenge,
    receipt: &AgentContextReceipt,
) -> WorkLedgerResult<()> {
    for value in [
        &receipt.reconstructed_context_digest,
        &receipt.agent_evidence_digest,
    ] {
        super::validate_digest("agent context evidence", value)?;
    }
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION
        || receipt.ownership_id != challenge.ownership_id
        || receipt.wake_id != challenge.wake_id
        || receipt.claim_id != challenge.claim_id
        || receipt.head_sha != challenge.head_sha
        || receipt.delivery_identity_digest != challenge.delivery_identity_digest
        || receipt.reconstructed_context_digest != challenge.expected_resume_context_digest
    {
        return Err(WorkLedgerError::Refused(
            "agent context receipt does not match its exact challenge".to_owned(),
        ));
    }
    Ok(())
}

fn validate_return_receipt(
    ownership: &AgentOwnership,
    receipt: &AgentReturnReceipt,
) -> WorkLedgerResult<()> {
    for value in [
        &receipt.context_receipt_digest,
        &receipt.next_checkpoint_digest,
        &receipt.evidence_digest,
        &receipt.remote_acknowledgement_digest,
    ] {
        super::validate_digest("agent return evidence", value)?;
    }
    if receipt.schema_version != RECEIPT_SCHEMA_VERSION
        || receipt.ownership_id != ownership.challenge.ownership_id
        || receipt.context_receipt_digest != ownership.context_receipt_digest
    {
        return Err(WorkLedgerError::Refused(
            "agent return receipt does not match acknowledged ownership".to_owned(),
        ));
    }
    Ok(())
}
