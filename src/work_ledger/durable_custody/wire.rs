//! Durable, transport-neutral custody transfer between Shipyard hosts.
//!
//! Transport is deliberately at-least-once. The receiver persists an inbox row
//! before returning a custody receipt, and applies the resulting ledger effect
//! in the same transaction that records the processed receipt. Terminal
//! adapters (cmux, `HerdR`, or future adapters) remain opaque routing metadata.

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};

/// Redacted durable custody lifecycle counts. "Read" is intentionally absent:
/// only storage and consumer-claim transitions are observable.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub(crate) struct CustodyStatus {
    pub(crate) outgoing_pending: u64,
    pub(crate) outgoing_claimed: u64,
    pub(crate) outgoing_accepted: u64,
    pub(crate) outgoing_processed: u64,
    pub(crate) outgoing_cancelled: u64,
    pub(crate) outgoing_superseded: u64,
    pub(crate) incoming_received: u64,
    pub(crate) incoming_processing: u64,
    pub(crate) incoming_processed: u64,
    pub(crate) incoming_cancelled: u64,
    pub(crate) incoming_superseded: u64,
    pub(crate) pending_controls: u64,
    pub(crate) pending_rebinds: u64,
}

use super::{
    WorkLedgerError, WorkLedgerResult, digest, opaque_ref, validate_control, validate_digest,
    validate_opaque_ref, validate_transfer,
};

const CUSTODY_SCHEMA_VERSION: u32 = 2;
pub(super) const MAX_LEASE: ChronoDuration = ChronoDuration::minutes(5);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CustodyKind {
    Wake,
    Correction,
    Followup,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyRelation {
    pub(crate) kind: CustodyKind,
    pub(crate) prior_message_id: Option<String>,
}

impl CustodyRelation {
    pub(crate) fn wake() -> Self {
        Self {
            kind: CustodyKind::Wake,
            prior_message_id: None,
        }
    }

    pub(crate) fn correction(prior_message_id: String) -> Self {
        Self {
            kind: CustodyKind::Correction,
            prior_message_id: Some(prior_message_id),
        }
    }

    pub(crate) fn followup(prior_message_id: String) -> Self {
        Self {
            kind: CustodyKind::Followup,
            prior_message_id: Some(prior_message_id),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyEnvelope {
    pub(crate) schema_version: u32,
    pub(crate) message_id: String,
    pub(crate) wake_id: String,
    pub(crate) work_item_id: String,
    pub(crate) work_generation: u64,
    pub(crate) owner_generation: u64,
    pub(crate) content_digest: String,
    pub(crate) work_authority_digest: String,
    pub(crate) workstream_handle: String,
    pub(crate) workstream_revision: u64,
    pub(crate) source_machine_ref: String,
    pub(crate) source_incarnation_ref: String,
    pub(crate) relation: CustodyRelation,
    pub(crate) identity_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyTransfer {
    pub(crate) envelope: CustodyEnvelope,
    pub(crate) rebind_epoch: u64,
    pub(crate) target_machine_ref: String,
    pub(crate) target_incarnation_ref: String,
    pub(crate) target_route_ref: String,
    pub(crate) terminal_adapter: String,
    pub(crate) rebind_authority_digest: String,
    pub(crate) transfer_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyReceipt {
    pub(crate) message_id: String,
    pub(crate) identity_digest: String,
    pub(crate) rebind_epoch: u64,
    pub(crate) target_incarnation_ref: String,
    pub(crate) transfer_digest: String,
    pub(crate) receipt_digest: String,
}

/// Authenticated successor-incarnation migration for custody already committed
/// by the destination but not yet processed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodySuccessorRebind {
    pub(crate) rebind_id: String,
    pub(crate) message_id: String,
    pub(crate) identity_digest: String,
    pub(crate) workstream_revision: u64,
    pub(crate) source_machine_ref: String,
    pub(crate) target_machine_ref: String,
    pub(crate) old_target_incarnation_ref: String,
    pub(crate) new_target_incarnation_ref: String,
    pub(crate) old_authority_epoch: u64,
    pub(crate) new_authority_epoch: u64,
    pub(crate) old_transfer_digest: String,
    pub(crate) old_custody_receipt_digest: String,
    pub(crate) new_target_route_ref: String,
    pub(crate) terminal_adapter: String,
    pub(crate) new_authority_digest: String,
    pub(crate) successor_proof_digest: String,
    pub(crate) rebind_digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodySuccessorReceipt {
    pub(crate) rebind_id: String,
    pub(crate) message_id: String,
    pub(crate) identity_digest: String,
    pub(crate) workstream_revision: u64,
    pub(crate) target_machine_ref: String,
    pub(crate) new_target_incarnation_ref: String,
    pub(crate) new_authority_epoch: u64,
    pub(crate) rebind_digest: String,
    pub(crate) successor_proof_digest: String,
    pub(crate) receipt_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedCustodySuccessorRebind {
    pub(super) rebind: CustodySuccessorRebind,
    pub(super) authenticated_source_machine_ref: String,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_custody_successor_rebind<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    source_machine_ref: &str,
    rebind: CustodySuccessorRebind,
) -> WorkLedgerResult<AuthenticatedCustodySuccessorRebind> {
    super::validate_successor_rebind(&rebind)?;
    let witness = authenticator.authenticate(source_machine_ref, &rebind.rebind_digest)?;
    validate_digest("custody successor transport witness", &witness)?;
    Ok(AuthenticatedCustodySuccessorRebind {
        rebind,
        authenticated_source_machine_ref: source_machine_ref.to_owned(),
        transport_auth_digest: witness,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedCustodySuccessorReceipt {
    pub(super) receipt: CustodySuccessorReceipt,
    pub(super) authenticated_peer_machine_ref: String,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_custody_successor_receipt<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    peer_machine_ref: &str,
    receipt: CustodySuccessorReceipt,
) -> WorkLedgerResult<AuthenticatedCustodySuccessorReceipt> {
    super::validate_successor_receipt(&receipt)?;
    let witness = authenticator.authenticate(peer_machine_ref, &receipt.receipt_digest)?;
    validate_digest("custody successor receipt transport witness", &witness)?;
    Ok(AuthenticatedCustodySuccessorReceipt {
        receipt,
        authenticated_peer_machine_ref: peer_machine_ref.to_owned(),
        transport_auth_digest: witness,
    })
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedCustodyReceipt {
    pub(super) receipt: CustodyReceipt,
    pub(super) authenticated_peer_machine_ref: String,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_custody_receipt<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    peer_machine_ref: &str,
    receipt: CustodyReceipt,
) -> WorkLedgerResult<AuthenticatedCustodyReceipt> {
    let witness = authenticator.authenticate(peer_machine_ref, &receipt.receipt_digest)?;
    validate_digest("custody receipt transport witness", &witness)?;
    Ok(AuthenticatedCustodyReceipt {
        receipt,
        authenticated_peer_machine_ref: peer_machine_ref.to_owned(),
        transport_auth_digest: witness,
    })
}

/// Authentication is supplied by the concrete cross-host transport. A hash
/// proves integrity only; this witness proves which authenticated peer sent it.
pub(crate) trait CustodyTransportAuthenticator {
    fn authenticate(
        &mut self,
        peer_machine_ref: &str,
        payload_digest: &str,
    ) -> WorkLedgerResult<String>;
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedCustodyTransfer {
    pub(super) transfer: CustodyTransfer,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_custody_transfer<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    transfer: CustodyTransfer,
) -> WorkLedgerResult<AuthenticatedCustodyTransfer> {
    validate_transfer(&transfer)?;
    let witness = authenticator.authenticate(
        &transfer.envelope.source_machine_ref,
        &transfer.transfer_digest,
    )?;
    validate_digest("custody transport authentication witness", &witness)?;
    Ok(AuthenticatedCustodyTransfer {
        transfer,
        transport_auth_digest: witness,
    })
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum CustodyControlKind {
    Cancelled,
    Superseded,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyControl {
    pub(crate) control_id: String,
    pub(crate) message_id: String,
    pub(crate) identity_digest: String,
    pub(super) kind: CustodyControlKind,
    pub(crate) successor_message_id: Option<String>,
    pub(crate) expected_rebind_epoch: u64,
    pub(crate) workstream_revision: u64,
    pub(crate) authority_digest: String,
    pub(crate) control_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedCustodyControl {
    pub(super) control: CustodyControl,
    pub(super) authenticated_source_machine_ref: String,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_custody_control<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    source_machine_ref: &str,
    control: CustodyControl,
) -> WorkLedgerResult<AuthenticatedCustodyControl> {
    validate_control(&control)?;
    let witness = authenticator.authenticate(source_machine_ref, &control.control_digest)?;
    validate_digest("custody control transport witness", &witness)?;
    Ok(AuthenticatedCustodyControl {
        control,
        authenticated_source_machine_ref: source_machine_ref.to_owned(),
        transport_auth_digest: witness,
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyControlReceipt {
    pub(crate) control_id: String,
    pub(crate) message_id: String,
    pub(crate) control_digest: String,
    pub(crate) terminal_state: String,
    pub(crate) receipt_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedCustodyControlReceipt {
    pub(super) receipt: CustodyControlReceipt,
    pub(super) authenticated_peer_machine_ref: String,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_custody_control_receipt<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    peer_machine_ref: &str,
    receipt: CustodyControlReceipt,
) -> WorkLedgerResult<AuthenticatedCustodyControlReceipt> {
    let witness = authenticator.authenticate(peer_machine_ref, &receipt.receipt_digest)?;
    validate_digest("custody control receipt transport witness", &witness)?;
    Ok(AuthenticatedCustodyControlReceipt {
        receipt,
        authenticated_peer_machine_ref: peer_machine_ref.to_owned(),
        transport_auth_digest: witness,
    })
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessedReceipt {
    pub(crate) message_id: String,
    pub(crate) identity_digest: String,
    pub(crate) workstream_revision: u64,
    pub(crate) effect_digest: String,
    pub(crate) target_machine_ref: String,
    pub(crate) target_incarnation_ref: String,
    pub(crate) rebind_epoch: u64,
    pub(crate) transfer_digest: String,
    pub(crate) authority_digest: String,
    pub(crate) consumer_owner_ref: String,
    pub(crate) consumer_epoch: u64,
    pub(crate) receipt_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct AuthenticatedProcessedReceipt {
    pub(super) receipt: ProcessedReceipt,
    pub(super) authenticated_peer_machine_ref: String,
    pub(super) transport_auth_digest: String,
}

pub(crate) fn authenticate_processed_receipt<A: CustodyTransportAuthenticator>(
    authenticator: &mut A,
    peer_machine_ref: &str,
    receipt: ProcessedReceipt,
) -> WorkLedgerResult<AuthenticatedProcessedReceipt> {
    let witness = authenticator.authenticate(peer_machine_ref, &receipt.receipt_digest)?;
    validate_digest("processed receipt transport witness", &witness)?;
    Ok(AuthenticatedProcessedReceipt {
        receipt,
        authenticated_peer_machine_ref: peer_machine_ref.to_owned(),
        transport_auth_digest: witness,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SenderClaim {
    pub(crate) message_id: String,
    pub(crate) epoch: u64,
    pub(crate) owner_ref: String,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InboxClaim {
    pub(crate) message_id: String,
    pub(crate) epoch: u64,
    pub(crate) owner_ref: String,
    pub(crate) expires_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InboxAuthority {
    pub(crate) workstream_revision: u64,
    pub(crate) target_incarnation_ref: String,
    pub(crate) authority_digest: String,
}

impl InboxAuthority {
    pub(crate) fn new(
        workstream_revision: u64,
        target_incarnation_ref: String,
        authority_digest: String,
    ) -> WorkLedgerResult<Self> {
        if workstream_revision == 0 {
            return Err(WorkLedgerError::Refused(
                "workstream revision must be positive".to_owned(),
            ));
        }
        validate_opaque_ref("target incarnation", &target_incarnation_ref, "incarnation")?;
        validate_digest("inbox authority", &authority_digest)?;
        Ok(Self {
            workstream_revision,
            target_incarnation_ref,
            authority_digest,
        })
    }
}

#[derive(Serialize)]
pub(super) struct EnvelopeIdentity<'a> {
    schema_version: u32,
    wake_id: &'a str,
    work_item_id: &'a str,
    work_generation: u64,
    owner_generation: u64,
    content_digest: &'a str,
    work_authority_digest: &'a str,
    workstream_handle: &'a str,
    workstream_revision: u64,
    source_machine_ref: &'a str,
    source_incarnation_ref: &'a str,
    relation: &'a CustodyRelation,
}

impl CustodyEnvelope {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        wake_id: String,
        work_item_id: String,
        work_generation: u64,
        owner_generation: u64,
        content_digest: String,
        work_authority_digest: String,
        workstream_handle: String,
        workstream_revision: u64,
        source_machine_ref: String,
        source_incarnation_ref: String,
        relation: CustodyRelation,
    ) -> WorkLedgerResult<Self> {
        validate_opaque_ref("wake ID", &wake_id, "wake")?;
        validate_opaque_ref("work item ID", &work_item_id, "wi")?;
        validate_digest("custody content", &content_digest)?;
        validate_digest("custody work authority", &work_authority_digest)?;
        super::super::validate_workstream_handle(&workstream_handle)?;
        validate_opaque_ref("source machine", &source_machine_ref, "machine")?;
        validate_opaque_ref("source incarnation", &source_incarnation_ref, "incarnation")?;
        if work_generation == 0 || owner_generation == 0 || workstream_revision == 0 {
            return Err(WorkLedgerError::Refused(
                "custody generations and revision must be positive".to_owned(),
            ));
        }
        match (&relation.kind, &relation.prior_message_id) {
            (CustodyKind::Wake, None) => {}
            (CustodyKind::Correction | CustodyKind::Followup, Some(id)) => {
                validate_opaque_ref("prior custody message", id, "wm")?;
            }
            _ => {
                return Err(WorkLedgerError::Refused(
                    "custody relation is structurally invalid".to_owned(),
                ));
            }
        }
        let identity = EnvelopeIdentity {
            schema_version: CUSTODY_SCHEMA_VERSION,
            wake_id: &wake_id,
            work_item_id: &work_item_id,
            work_generation,
            owner_generation,
            content_digest: &content_digest,
            work_authority_digest: &work_authority_digest,
            workstream_handle: &workstream_handle,
            workstream_revision,
            source_machine_ref: &source_machine_ref,
            source_incarnation_ref: &source_incarnation_ref,
            relation: &relation,
        };
        let encoded = serde_json::to_vec(&identity).map_err(|_| {
            WorkLedgerError::Refused("custody identity cannot be serialized".to_owned())
        })?;
        let identity_digest = digest(&encoded);
        let message_id = opaque_ref("wm", &identity_digest);
        Ok(Self {
            schema_version: CUSTODY_SCHEMA_VERSION,
            message_id,
            wake_id,
            work_item_id,
            work_generation,
            owner_generation,
            content_digest,
            work_authority_digest,
            workstream_handle,
            workstream_revision,
            source_machine_ref,
            source_incarnation_ref,
            relation,
            identity_digest,
        })
    }

    pub(crate) fn validate(&self) -> WorkLedgerResult<()> {
        let rebuilt = Self::new(
            self.wake_id.clone(),
            self.work_item_id.clone(),
            self.work_generation,
            self.owner_generation,
            self.content_digest.clone(),
            self.work_authority_digest.clone(),
            self.workstream_handle.clone(),
            self.workstream_revision,
            self.source_machine_ref.clone(),
            self.source_incarnation_ref.clone(),
            self.relation.clone(),
        )?;
        if rebuilt != *self {
            return Err(WorkLedgerError::Refused(
                "custody envelope identity is invalid".to_owned(),
            ));
        }
        Ok(())
    }
}
