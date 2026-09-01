use super::{
    Connection, CustodyControl, CustodyControlKind, CustodyControlReceipt, CustodyEnvelope,
    CustodyReceipt, CustodyRelation, CustodySuccessorRebind, CustodySuccessorReceipt,
    CustodyTransfer, DateTime, InboxClaim, MAX_LEASE, OptionalExtension, ProcessedReceipt,
    SenderClaim, Transaction, TransactionBehavior, Utc, WorkLedger, WorkLedgerError,
    WorkLedgerResult, configure_durable, digest, opaque_ref, params, validate_digest,
    validate_opaque_ref, validate_token, verify_integrity, verify_supported_schema,
};

#[allow(clippy::too_many_lines)] // Reopen validation keeps effect, receipt, and ack invariants together.
pub(super) fn validate_persisted_processed_receipts(
    connection: &Connection,
) -> WorkLedgerResult<()> {
    let invalid_effect_state: i64 = connection.query_row(
        "SELECT COUNT(*)
           FROM custody_inbox inbox
           LEFT JOIN custody_effects effect ON effect.message_id = inbox.message_id
          WHERE (inbox.state = 'processed' AND
                 (effect.message_id IS NULL OR effect.effect_digest != inbox.effect_digest))
             OR (inbox.state != 'processed' AND effect.message_id IS NOT NULL)",
        [],
        |row| row.get(0),
    )?;
    if invalid_effect_state != 0 {
        return Err(WorkLedgerError::Refused(
            "custody effect state is not exact".to_owned(),
        ));
    }

    let mut statement = connection.prepare(
        "SELECT inbox.identity_json, inbox.target_machine_ref, inbox.effect_digest,
                inbox.processed_receipt_digest, inbox.processed_receipt_json
           FROM custody_inbox inbox WHERE inbox.state = 'processed'",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, Vec<u8>>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    for row in rows {
        let (identity_json, machine, effect, receipt_digest, receipt_json) = row?;
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        envelope.validate()?;
        let active = super::active_inbox_binding(connection, &envelope.message_id)?;
        let receipt: ProcessedReceipt = serde_json::from_slice(&receipt_json).map_err(|_| {
            WorkLedgerError::Refused("stored processed receipt is invalid".to_owned())
        })?;
        validate_processed_receipt(&receipt)?;
        let claim_exists: i64 = connection.query_row(
            "SELECT COUNT(*) FROM custody_inbox_claims
              WHERE message_id = ?1 AND epoch = ?2 AND owner_ref = ?3",
            params![
                receipt.message_id,
                receipt.consumer_epoch,
                receipt.consumer_owner_ref
            ],
            |query_row| query_row.get(0),
        )?;
        if receipt.receipt_digest != receipt_digest
            || receipt.message_id != envelope.message_id
            || receipt.identity_digest != envelope.identity_digest
            || receipt.workstream_revision != envelope.workstream_revision
            || receipt.effect_digest != effect
            || receipt.target_machine_ref != machine
            || receipt.target_incarnation_ref != active.incarnation
            || receipt.rebind_epoch != active.epoch
            || receipt.transfer_digest != active.transfer_digest
            || receipt.authority_digest != active.authority_digest
            || claim_exists != 1
        {
            return Err(WorkLedgerError::Refused(
                "stored processed receipt does not match custody state".to_owned(),
            ));
        }
    }
    let acknowledgements_exist: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
          WHERE type = 'table' AND name = 'custody_processed_acknowledgements')",
        [],
        |row| row.get(0),
    )?;
    if !acknowledgements_exist {
        return Ok(());
    }
    let mut acknowledgements = connection.prepare(
        "SELECT ack.receipt_digest, ack.message_id, ack.source_machine_ref,
                inbox.processed_receipt_digest, inbox.identity_json, inbox.state
           FROM custody_processed_acknowledgements ack
           JOIN custody_inbox inbox ON inbox.message_id = ack.message_id",
    )?;
    let rows = acknowledgements.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (ack_digest, message_id, source_machine, receipt_digest, identity_json, state) = row?;
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        envelope.validate()?;
        if state != "processed"
            || ack_digest != receipt_digest
            || message_id != envelope.message_id
            || source_machine != envelope.source_machine_ref
        {
            return Err(WorkLedgerError::Refused(
                "stored processed acknowledgement contradicts custody state".to_owned(),
            ));
        }
    }
    Ok(())
}

impl WorkLedger {
    pub(crate) fn cancel_or_supersede_unprocessed_custody(
        &self,
        message_id: &str,
        successor: Option<&str>,
        authority_digest: &str,
    ) -> WorkLedgerResult<()> {
        validate_digest("custody control authority", authority_digest)?;
        if let Some(value) = successor {
            validate_opaque_ref("custody successor", value, "wm")?;
        }
        let target_state = if successor.is_some() {
            "superseded"
        } else {
            "cancelled"
        };
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let table = if tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM custody_outbox WHERE message_id = ?1)",
            [message_id],
            |row| row.get::<_, bool>(0),
        )? {
            "custody_outbox"
        } else {
            "custody_inbox"
        };
        let state: String = tx.query_row(
            &format!("SELECT state FROM {table} WHERE message_id = ?1"),
            [message_id],
            |row| row.get(0),
        )?;
        let cancellable = (table == "custody_outbox" && state == "pending")
            || (table == "custody_inbox" && state == "received");
        if !cancellable {
            return Err(WorkLedgerError::Refused(
                "claimed, processed, or remotely-owned custody requires fenced reconciliation"
                    .to_owned(),
            ));
        }
        tx.execute(
            &format!("UPDATE {table} SET state = ?2, updated_at = ?3 WHERE message_id = ?1"),
            params![message_id, target_state, Utc::now().to_rfc3339()],
        )?;
        let side = if table == "custody_outbox" {
            "sender"
        } else {
            "receiver"
        };
        record_custody_event(
            &tx,
            message_id,
            side,
            target_state,
            authority_digest,
            &Utc::now().to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(super) fn custody_write_connection(
        &self,
    ) -> WorkLedgerResult<(
        Option<crate::writer_domain_lease::ProductionWriterDomainLease>,
        rusqlite::Connection,
    )> {
        let parent = self.path.parent().ok_or_else(|| {
            WorkLedgerError::Refused("custody ledger database has no parent".to_owned())
        })?;
        let writer_domain = crate::writer_domain_lease::acquire_for_protected_path(parent)?;
        let connection = self.connect_read_write()?;
        configure_durable(&connection)?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        Ok((writer_domain, connection))
    }

    pub(super) fn custody_read_connection(&self) -> WorkLedgerResult<rusqlite::Connection> {
        let connection = self.connect_read_only()?;
        verify_supported_schema(&connection)?;
        verify_integrity(&connection)?;
        Ok(connection)
    }
}

pub(super) fn relation_prior_state(
    tx: &Transaction<'_>,
    relation: &CustodyRelation,
) -> WorkLedgerResult<Option<String>> {
    relation
        .prior_message_id
        .as_deref()
        .map(|id| {
            tx.query_row(
                "SELECT state FROM custody_outbox WHERE message_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
        })
        .transpose()
        .map(Option::flatten)
        .map_err(Into::into)
}

pub(super) fn validate_target(
    machine: &str,
    incarnation: &str,
    route: &str,
    adapter: &str,
    authority: &str,
) -> WorkLedgerResult<()> {
    validate_opaque_ref("target machine", machine, "machine")?;
    validate_opaque_ref("target incarnation", incarnation, "incarnation")?;
    validate_opaque_ref("target route", route, "route")?;
    validate_token("terminal adapter", adapter)?;
    validate_digest("target rebind authority", authority)
}

pub(super) fn validate_lease(now: DateTime<Utc>, expiry: DateTime<Utc>) -> WorkLedgerResult<()> {
    if expiry <= now || expiry - now > MAX_LEASE {
        return Err(WorkLedgerError::Refused(
            "custody lease is outside the permitted window".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Every cross-host fence is part of the digest contract.
pub(super) fn transfer_digest(
    envelope: &CustodyEnvelope,
    epoch: u64,
    machine: &str,
    incarnation: &str,
    route: &str,
    adapter: &str,
    authority: &str,
) -> WorkLedgerResult<String> {
    let encoded = serde_json::to_vec(&(
        envelope,
        epoch,
        machine,
        incarnation,
        route,
        adapter,
        authority,
    ))
    .map_err(|_| WorkLedgerError::Refused("custody transfer cannot be serialized".to_owned()))?;
    Ok(digest(&encoded))
}

pub(super) fn validate_transfer(transfer: &CustodyTransfer) -> WorkLedgerResult<()> {
    transfer.envelope.validate()?;
    validate_target(
        &transfer.target_machine_ref,
        &transfer.target_incarnation_ref,
        &transfer.target_route_ref,
        &transfer.terminal_adapter,
        &transfer.rebind_authority_digest,
    )?;
    let expected = transfer_digest(
        &transfer.envelope,
        transfer.rebind_epoch,
        &transfer.target_machine_ref,
        &transfer.target_incarnation_ref,
        &transfer.target_route_ref,
        &transfer.terminal_adapter,
        &transfer.rebind_authority_digest,
    )?;
    if expected != transfer.transfer_digest {
        return Err(WorkLedgerError::Refused(
            "custody transfer digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn control_kind_str(kind: CustodyControlKind) -> &'static str {
    match kind {
        CustodyControlKind::Cancelled => "cancelled",
        CustodyControlKind::Superseded => "superseded",
    }
}

pub(super) fn validate_control(control: &CustodyControl) -> WorkLedgerResult<()> {
    validate_opaque_ref("custody control ID", &control.control_id, "cc")?;
    validate_opaque_ref("custody control message", &control.message_id, "wm")?;
    validate_digest("custody control identity", &control.identity_digest)?;
    validate_digest("custody control authority", &control.authority_digest)?;
    if control.expected_rebind_epoch == 0 || control.workstream_revision == 0 {
        return Err(WorkLedgerError::Refused(
            "custody control epochs and revisions must be positive".to_owned(),
        ));
    }
    match (control.kind, control.successor_message_id.as_deref()) {
        (CustodyControlKind::Cancelled, None) => {}
        (CustodyControlKind::Superseded, Some(successor)) => {
            validate_opaque_ref("custody control successor", successor, "wm")?;
        }
        _ => {
            return Err(WorkLedgerError::Refused(
                "custody control relation is invalid".to_owned(),
            ));
        }
    }
    let expected = digest(
        &serde_json::to_vec(&(
            control.message_id.as_str(),
            control.identity_digest.as_str(),
            control.kind,
            control.successor_message_id.as_deref(),
            control.expected_rebind_epoch,
            control.workstream_revision,
            control.authority_digest.as_str(),
        ))
        .map_err(|_| WorkLedgerError::Refused("custody control cannot be serialized".to_owned()))?,
    );
    if expected != control.control_digest || opaque_ref("cc", &expected) != control.control_id {
        return Err(WorkLedgerError::Refused(
            "custody control digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn control_receipt(control: &CustodyControl) -> WorkLedgerResult<CustodyControlReceipt> {
    let mut receipt = CustodyControlReceipt {
        control_id: control.control_id.clone(),
        message_id: control.message_id.clone(),
        control_digest: control.control_digest.clone(),
        terminal_state: control_kind_str(control.kind).to_owned(),
        receipt_digest: String::new(),
    };
    receipt.receipt_digest = digest(&serde_json::to_vec(&receipt).map_err(|_| {
        WorkLedgerError::Refused("custody control receipt cannot be serialized".to_owned())
    })?);
    Ok(receipt)
}

pub(super) fn validate_control_receipt(receipt: &CustodyControlReceipt) -> WorkLedgerResult<()> {
    if !matches!(receipt.terminal_state.as_str(), "cancelled" | "superseded") {
        return Err(WorkLedgerError::Refused(
            "custody control receipt state is invalid".to_owned(),
        ));
    }
    let mut unsigned = receipt.clone();
    unsigned.receipt_digest.clear();
    let expected = digest(&serde_json::to_vec(&unsigned).map_err(|_| {
        WorkLedgerError::Refused("custody control receipt cannot be serialized".to_owned())
    })?);
    if expected != receipt.receipt_digest {
        return Err(WorkLedgerError::Refused(
            "custody control receipt digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn load_control(tx: &Connection, control_id: &str) -> WorkLedgerResult<CustodyControl> {
    let row: (
        String,
        String,
        String,
        Option<String>,
        i64,
        i64,
        String,
        String,
    ) = tx.query_row(
        "SELECT message_id, identity_digest, kind, successor_message_id,
                expected_rebind_epoch, workstream_revision, authority_digest, control_digest
           FROM custody_controls WHERE control_id = ?1",
        [control_id],
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
    )?;
    let kind = match row.2.as_str() {
        "cancelled" => CustodyControlKind::Cancelled,
        "superseded" => CustodyControlKind::Superseded,
        _ => {
            return Err(WorkLedgerError::Refused(
                "stored custody control is invalid".to_owned(),
            ));
        }
    };
    let control = CustodyControl {
        control_id: control_id.to_owned(),
        message_id: row.0,
        identity_digest: row.1,
        kind,
        successor_message_id: row.3,
        expected_rebind_epoch: positive_u64("stored control rebind epoch", row.4)?,
        workstream_revision: positive_u64("stored control workstream revision", row.5)?,
        authority_digest: row.6,
        control_digest: row.7,
    };
    validate_control(&control)?;
    Ok(control)
}

pub(super) fn receipt_from_transfer(
    transfer: &CustodyTransfer,
    stored_digest: Option<&str>,
) -> WorkLedgerResult<CustodyReceipt> {
    let mut receipt = CustodyReceipt {
        message_id: transfer.envelope.message_id.clone(),
        identity_digest: transfer.envelope.identity_digest.clone(),
        rebind_epoch: transfer.rebind_epoch,
        target_incarnation_ref: transfer.target_incarnation_ref.clone(),
        transfer_digest: transfer.transfer_digest.clone(),
        receipt_digest: String::new(),
    };
    receipt.receipt_digest = digest(&serde_json::to_vec(&receipt).map_err(|_| {
        WorkLedgerError::Refused("custody receipt cannot be serialized".to_owned())
    })?);
    if stored_digest.is_some_and(|stored| stored != receipt.receipt_digest) {
        return Err(WorkLedgerError::Refused(
            "stored custody receipt is inconsistent".to_owned(),
        ));
    }
    Ok(receipt)
}

pub(super) fn validate_receipt(receipt: &CustodyReceipt) -> WorkLedgerResult<()> {
    let mut unsigned = receipt.clone();
    unsigned.receipt_digest.clear();
    let expected = digest(&serde_json::to_vec(&unsigned).map_err(|_| {
        WorkLedgerError::Refused("custody receipt cannot be serialized".to_owned())
    })?);
    if expected != receipt.receipt_digest {
        return Err(WorkLedgerError::Refused(
            "custody receipt digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

fn validate_successor_lease_binding(rebind: &CustodySuccessorRebind) -> WorkLedgerResult<()> {
    validate_digest("custody successor proof", &rebind.successor_proof_digest)?;
    validate_opaque_ref(
        "custody successor ownership lease",
        &rebind.ownership_lease_id,
        "ol",
    )?;
    validate_opaque_ref(
        "custody successor holder",
        &rebind.successor_holder_ref,
        "owner",
    )?;
    validate_opaque_ref(
        "custody successor holder session",
        &rebind.successor_session_incarnation_ref,
        "incarnation",
    )
}

pub(super) fn validate_successor_rebind(rebind: &CustodySuccessorRebind) -> WorkLedgerResult<()> {
    validate_opaque_ref("custody successor rebind", &rebind.rebind_id, "cr")?;
    validate_opaque_ref("custody successor message", &rebind.message_id, "wm")?;
    validate_digest("custody successor identity", &rebind.identity_digest)?;
    validate_opaque_ref(
        "custody successor source",
        &rebind.source_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "custody successor target",
        &rebind.target_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "old custody successor incarnation",
        &rebind.old_target_incarnation_ref,
        "incarnation",
    )?;
    validate_target(
        &rebind.target_machine_ref,
        &rebind.new_target_incarnation_ref,
        &rebind.new_target_route_ref,
        &rebind.terminal_adapter,
        &rebind.new_authority_digest,
    )?;
    validate_digest(
        "old custody successor transfer",
        &rebind.old_transfer_digest,
    )?;
    validate_digest(
        "old custody successor receipt",
        &rebind.old_custody_receipt_digest,
    )?;
    validate_successor_lease_binding(rebind)?;
    if rebind.workstream_revision == 0
        || rebind.old_authority_epoch == 0
        || rebind.ownership_lease_generation == 0
        || rebind.new_authority_epoch != rebind.old_authority_epoch.saturating_add(1)
        || rebind.pull_request == 0
        || rebind.ownership_root_uuid.len() != 36
        || rebind.repository_provider.is_empty()
        || rebind.repository_id.is_empty()
        || !super::super::is_canonical_repo_slug(&rebind.repository)
        || rebind.exact_head.len() != 40
        || rebind
            .exact_head
            .bytes()
            .any(|byte| !byte.is_ascii_hexdigit() || byte.is_ascii_uppercase())
        || rebind.workstream_handle.is_empty()
    {
        return Err(WorkLedgerError::Refused(
            "custody successor epochs or incarnations are invalid".to_owned(),
        ));
    }
    let expected = digest(
        &serde_json::to_vec(&(
            rebind.message_id.as_str(),
            rebind.identity_digest.as_str(),
            rebind.workstream_revision,
            rebind.source_machine_ref.as_str(),
            rebind.target_machine_ref.as_str(),
            rebind.old_target_incarnation_ref.as_str(),
            rebind.new_target_incarnation_ref.as_str(),
            rebind.old_authority_epoch,
            rebind.new_authority_epoch,
            rebind.old_transfer_digest.as_str(),
            rebind.old_custody_receipt_digest.as_str(),
            rebind.new_target_route_ref.as_str(),
            rebind.terminal_adapter.as_str(),
            rebind.new_authority_digest.as_str(),
            (
                rebind.ownership_lease_id.as_str(),
                rebind.ownership_lease_generation,
                rebind.ownership_lease_expires_at.to_rfc3339(),
                rebind.ownership_root_uuid.as_str(),
                rebind.repository_provider.as_str(),
                rebind.repository_id.as_str(),
                rebind.repository.as_str(),
                rebind.pull_request,
                rebind.exact_head.as_str(),
                rebind.workstream_handle.as_str(),
                rebind.successor_holder_ref.as_str(),
                rebind.successor_session_incarnation_ref.as_str(),
            ),
            rebind.successor_proof_digest.as_str(),
        ))
        .map_err(|_| {
            WorkLedgerError::Refused("custody successor cannot be serialized".to_owned())
        })?,
    );
    if rebind.rebind_digest != expected || rebind.rebind_id != opaque_ref("cr", &expected) {
        return Err(WorkLedgerError::Refused(
            "custody successor digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn successor_receipt(
    rebind: &CustodySuccessorRebind,
) -> WorkLedgerResult<CustodySuccessorReceipt> {
    let mut receipt = CustodySuccessorReceipt {
        rebind_id: rebind.rebind_id.clone(),
        message_id: rebind.message_id.clone(),
        identity_digest: rebind.identity_digest.clone(),
        workstream_revision: rebind.workstream_revision,
        target_machine_ref: rebind.target_machine_ref.clone(),
        new_target_incarnation_ref: rebind.new_target_incarnation_ref.clone(),
        new_authority_epoch: rebind.new_authority_epoch,
        rebind_digest: rebind.rebind_digest.clone(),
        successor_proof_digest: rebind.successor_proof_digest.clone(),
        receipt_digest: String::new(),
    };
    receipt.receipt_digest = digest(&serde_json::to_vec(&receipt).map_err(|_| {
        WorkLedgerError::Refused("custody successor receipt cannot be serialized".to_owned())
    })?);
    Ok(receipt)
}

pub(super) fn validate_successor_receipt(
    receipt: &CustodySuccessorReceipt,
) -> WorkLedgerResult<()> {
    validate_opaque_ref("custody successor receipt", &receipt.rebind_id, "cr")?;
    validate_opaque_ref("custody successor message", &receipt.message_id, "wm")?;
    validate_digest("custody successor identity", &receipt.identity_digest)?;
    validate_opaque_ref(
        "custody successor target",
        &receipt.target_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "custody successor incarnation",
        &receipt.new_target_incarnation_ref,
        "incarnation",
    )?;
    validate_digest("custody successor rebind", &receipt.rebind_digest)?;
    validate_digest("custody successor proof", &receipt.successor_proof_digest)?;
    let mut unsigned = receipt.clone();
    unsigned.receipt_digest.clear();
    let expected = digest(&serde_json::to_vec(&unsigned).map_err(|_| {
        WorkLedgerError::Refused("custody successor receipt cannot be serialized".to_owned())
    })?);
    if receipt.workstream_revision == 0
        || receipt.new_authority_epoch == 0
        || expected != receipt.receipt_digest
    {
        return Err(WorkLedgerError::Refused(
            "custody successor receipt is invalid".to_owned(),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // Receipt construction names every independently checked fence.
pub(super) fn processed_receipt(
    envelope: &CustodyEnvelope,
    effect: &str,
    target_machine_ref: &str,
    target_incarnation_ref: &str,
    rebind_epoch: u64,
    transfer_digest: &str,
    authority_digest: &str,
    claim: &InboxClaim,
) -> WorkLedgerResult<ProcessedReceipt> {
    let mut receipt = ProcessedReceipt {
        message_id: envelope.message_id.clone(),
        identity_digest: envelope.identity_digest.clone(),
        workstream_revision: envelope.workstream_revision,
        effect_digest: effect.to_owned(),
        target_machine_ref: target_machine_ref.to_owned(),
        target_incarnation_ref: target_incarnation_ref.to_owned(),
        rebind_epoch,
        transfer_digest: transfer_digest.to_owned(),
        authority_digest: authority_digest.to_owned(),
        consumer_owner_ref: claim.owner_ref.clone(),
        consumer_epoch: claim.epoch,
        receipt_digest: String::new(),
    };
    receipt.receipt_digest = digest(&serde_json::to_vec(&receipt).map_err(|_| {
        WorkLedgerError::Refused("processed receipt cannot be serialized".to_owned())
    })?);
    Ok(receipt)
}

pub(super) fn validate_processed_receipt(receipt: &ProcessedReceipt) -> WorkLedgerResult<()> {
    validate_digest("processed effect", &receipt.effect_digest)?;
    validate_digest("processed transfer", &receipt.transfer_digest)?;
    validate_digest("processed authority", &receipt.authority_digest)?;
    validate_opaque_ref(
        "processed target machine",
        &receipt.target_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "processed target incarnation",
        &receipt.target_incarnation_ref,
        "incarnation",
    )?;
    validate_opaque_ref(
        "processed consumer owner",
        &receipt.consumer_owner_ref,
        "owner",
    )?;
    if receipt.rebind_epoch == 0 || receipt.consumer_epoch == 0 {
        return Err(WorkLedgerError::Refused(
            "processed receipt epochs must be positive".to_owned(),
        ));
    }
    let mut unsigned = receipt.clone();
    unsigned.receipt_digest.clear();
    let expected = digest(&serde_json::to_vec(&unsigned).map_err(|_| {
        WorkLedgerError::Refused("processed receipt cannot be serialized".to_owned())
    })?);
    if expected != receipt.receipt_digest {
        return Err(WorkLedgerError::Refused(
            "processed receipt digest is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn verify_sender_claim(
    connection: &rusqlite::Connection,
    claim: &SenderClaim,
    now: DateTime<Utc>,
) -> WorkLedgerResult<()> {
    let row: Option<(String, String, String)> = connection.query_row("SELECT owner_ref, state, expires_at FROM custody_sender_claims WHERE message_id = ?1 AND epoch = ?2", params![claim.message_id, claim.epoch], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let Some((owner, state, expiry)) = row else {
        return Err(WorkLedgerError::Refused(
            "custody sender claim is missing".to_owned(),
        ));
    };
    let expiry = DateTime::parse_from_rfc3339(&expiry)
        .map_err(|_| WorkLedgerError::Refused("custody sender lease is invalid".to_owned()))?
        .with_timezone(&Utc);
    if owner != claim.owner_ref || state != "active" || expiry != claim.expires_at || expiry <= now
    {
        return Err(WorkLedgerError::Refused(
            "custody sender claim is stale".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn verify_inbox_claim(
    connection: &rusqlite::Connection,
    claim: &InboxClaim,
    now: DateTime<Utc>,
) -> WorkLedgerResult<()> {
    let row: Option<(String, String, String)> = connection.query_row("SELECT owner_ref, state, expires_at FROM custody_inbox_claims WHERE message_id = ?1 AND epoch = ?2", params![claim.message_id, claim.epoch], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).optional()?;
    let Some((owner, state, expiry)) = row else {
        return Err(WorkLedgerError::Refused(
            "custody inbox claim is missing".to_owned(),
        ));
    };
    let expiry = DateTime::parse_from_rfc3339(&expiry)
        .map_err(|_| WorkLedgerError::Refused("custody inbox lease is invalid".to_owned()))?
        .with_timezone(&Utc);
    if owner != claim.owner_ref || state != "active" || expiry != claim.expires_at || expiry <= now
    {
        return Err(WorkLedgerError::Refused(
            "custody inbox claim is stale".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn transfer_from_tx(
    tx: &Transaction<'_>,
    claim: &SenderClaim,
) -> WorkLedgerResult<CustodyTransfer> {
    let (identity_json, rebind_epoch): (Vec<u8>, i64) = tx.query_row(
        "SELECT identity_json, active_rebind_epoch FROM custody_outbox WHERE message_id = ?1",
        [&claim.message_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json)
        .map_err(|_| WorkLedgerError::Refused("stored custody envelope is invalid".to_owned()))?;
    let (machine, incarnation, route, adapter, authority): (String, String, String, String, String) = tx.query_row("SELECT target_machine_ref, target_incarnation_ref, target_route_ref, terminal_adapter, authority_digest FROM custody_rebinds WHERE message_id = ?1 AND epoch = ?2", params![claim.message_id, rebind_epoch], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)))?;
    let transfer_digest = transfer_digest(
        &envelope,
        positive_u64("stored transfer rebind epoch", rebind_epoch)?,
        &machine,
        &incarnation,
        &route,
        &adapter,
        &authority,
    )?;
    Ok(CustodyTransfer {
        envelope,
        rebind_epoch: positive_u64("stored transfer rebind epoch", rebind_epoch)?,
        target_machine_ref: machine,
        target_incarnation_ref: incarnation,
        target_route_ref: route,
        terminal_adapter: adapter,
        rebind_authority_digest: authority,
        transfer_digest,
    })
}

pub(super) fn release_sender_claim(
    tx: &Transaction<'_>,
    claim: &SenderClaim,
    now: &str,
) -> WorkLedgerResult<()> {
    let changed = tx.execute("UPDATE custody_sender_claims SET state = 'released', released_at = ?3 WHERE message_id = ?1 AND epoch = ?2 AND state = 'active'", params![claim.message_id, claim.epoch, now])?;
    if changed != 1 {
        return Err(WorkLedgerError::Refused(
            "custody sender claim was superseded".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn release_inbox_claim(
    tx: &Transaction<'_>,
    claim: &InboxClaim,
    now: &str,
) -> WorkLedgerResult<()> {
    let changed = tx.execute("UPDATE custody_inbox_claims SET state = 'released', released_at = ?3 WHERE message_id = ?1 AND epoch = ?2 AND state = 'active'", params![claim.message_id, claim.epoch, now])?;
    if changed != 1 {
        return Err(WorkLedgerError::Refused(
            "custody inbox claim was superseded".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn release_expired_claims(
    tx: &Transaction<'_>,
    table: &str,
    message_id: &str,
    now: DateTime<Utc>,
) -> WorkLedgerResult<()> {
    let sql = match table {
        "custody_sender_claims" => {
            "UPDATE custody_sender_claims SET state = 'released', released_at = ?2 WHERE message_id = ?1 AND state = 'active' AND expires_at <= ?2"
        }
        "custody_inbox_claims" => {
            "UPDATE custody_inbox_claims SET state = 'released', released_at = ?2 WHERE message_id = ?1 AND state = 'active' AND expires_at <= ?2"
        }
        _ => {
            return Err(WorkLedgerError::Refused(
                "unsupported custody claim table".to_owned(),
            ));
        }
    };
    tx.execute(sql, params![message_id, now.to_rfc3339()])?;
    Ok(())
}

pub(super) fn record_custody_event(
    tx: &Transaction<'_>,
    message_id: &str,
    side: &str,
    kind: &str,
    evidence: &str,
    now: &str,
) -> WorkLedgerResult<()> {
    validate_digest("custody event evidence", evidence)?;
    let sequence: i64 = tx.query_row("SELECT coalesce(max(sequence), 0) + 1 FROM custody_events WHERE message_id = ?1 AND side = ?2", params![message_id, side], |row| row.get(0))?;
    let event_id = opaque_ref(
        "ce",
        &format!("{message_id}\n{side}\n{sequence}\n{kind}\n{evidence}"),
    );
    tx.execute("INSERT INTO custody_events (event_id, message_id, side, sequence, kind, evidence_digest, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)", params![event_id, message_id, side, sequence, kind, evidence, now])?;
    Ok(())
}

pub(super) fn positive_u64(label: &str, value: i64) -> WorkLedgerResult<u64> {
    u64::try_from(value)
        .map_err(|_| WorkLedgerError::Refused(format!("{label} is outside the supported range")))
}

pub(super) fn sqlite_i64(label: &str, value: u64) -> WorkLedgerResult<i64> {
    i64::try_from(value)
        .map_err(|_| WorkLedgerError::Refused(format!("{label} is outside the supported range")))
}
