use super::{
    AuthenticatedCustodyControl, AuthenticatedCustodyTransfer, CustodyControlReceipt,
    CustodyEnvelope, CustodyReceipt, DateTime, InboxAuthority, InboxClaim, OptionalExtension,
    ProcessedReceipt, Transaction, TransactionBehavior, Utc, WorkLedger, WorkLedgerError,
    WorkLedgerResult, control_kind_str, control_receipt, digest, load_control, params,
    positive_u64, processed_receipt, receipt_from_transfer, record_custody_event,
    release_expired_claims, release_inbox_claim, sqlite_i64, validate_control, validate_digest,
    validate_lease, validate_opaque_ref, validate_transfer, verify_inbox_claim,
};

impl WorkLedger {
    pub(crate) fn accept_custody(
        &self,
        authenticated: &AuthenticatedCustodyTransfer,
        local_machine_ref: &str,
        local_incarnation_ref: &str,
    ) -> WorkLedgerResult<CustodyReceipt> {
        let transfer = &authenticated.transfer;
        validate_transfer(transfer)?;
        validate_opaque_ref("local custody machine", local_machine_ref, "machine")?;
        validate_opaque_ref(
            "local custody incarnation",
            local_incarnation_ref,
            "incarnation",
        )?;
        if transfer.target_machine_ref != local_machine_ref
            || transfer.target_incarnation_ref != local_incarnation_ref
        {
            return Err(WorkLedgerError::Refused(
                "custody transfer is addressed to a different machine incarnation".to_owned(),
            ));
        }
        validate_digest(
            "custody transport authentication witness",
            &authenticated.transport_auth_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(String, String)> = tx.query_row(
            "SELECT identity_digest, custody_receipt_digest FROM custody_inbox WHERE message_id = ?1",
            [&transfer.envelope.message_id], |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional()?;
        if let Some((identity, transfer_receipt)) = existing {
            if identity != transfer.envelope.identity_digest {
                return Err(WorkLedgerError::Refused(
                    "custody message ID collision".to_owned(),
                ));
            }
            return receipt_from_transfer(transfer, Some(&transfer_receipt));
        }
        let receipt = receipt_from_transfer(transfer, None)?;
        let now = Utc::now().to_rfc3339();
        let identity_json = serde_json::to_vec(&transfer.envelope).map_err(|_| {
            WorkLedgerError::Refused("custody envelope cannot be serialized".to_owned())
        })?;
        tx.execute(
            "INSERT INTO custody_inbox
             (message_id, identity_json, identity_digest, rebind_epoch, target_machine_ref,
              target_incarnation_ref, target_route_ref, terminal_adapter, authority_digest,
              transfer_digest, transport_auth_digest, state, custody_receipt_digest,
              received_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                     'received', ?12, ?13, ?13)",
            params![
                transfer.envelope.message_id,
                identity_json,
                transfer.envelope.identity_digest,
                transfer.rebind_epoch,
                transfer.target_machine_ref,
                transfer.target_incarnation_ref,
                transfer.target_route_ref,
                transfer.terminal_adapter,
                transfer.rebind_authority_digest,
                transfer.transfer_digest,
                authenticated.transport_auth_digest,
                receipt.receipt_digest,
                now
            ],
        )?;
        record_custody_event(
            &tx,
            &transfer.envelope.message_id,
            "receiver",
            "custody_accepted",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn claim_custody_inbox(
        &self,
        message_id: &str,
        owner_ref: &str,
        lease_expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<InboxClaim> {
        validate_opaque_ref("custody message", message_id, "wm")?;
        validate_opaque_ref("custody inbox owner", owner_ref, "owner")?;
        let now = Utc::now();
        validate_lease(now, lease_expires_at)?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        release_expired_claims(&tx, "custody_inbox_claims", message_id, now)?;
        let state: String = tx.query_row(
            "SELECT state FROM custody_inbox WHERE message_id = ?1",
            [message_id],
            |row| row.get(0),
        )?;
        if state != "received" && state != "processing" {
            return Err(WorkLedgerError::Refused(
                "custody inbox message is not claimable".to_owned(),
            ));
        }
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM custody_inbox_claims WHERE message_id = ?1 AND state = 'active'",
            [message_id],
            |row| row.get(0),
        )?;
        if active != 0 {
            return Err(WorkLedgerError::Refused(
                "custody inbox already has an active consumer".to_owned(),
            ));
        }
        let epoch: i64 = tx.query_row(
            "SELECT coalesce(max(epoch), 0) + 1 FROM custody_inbox_claims WHERE message_id = ?1",
            [message_id],
            |row| row.get(0),
        )?;
        tx.execute("INSERT INTO custody_inbox_claims (message_id, epoch, owner_ref, state, acquired_at, expires_at) VALUES (?1, ?2, ?3, 'active', ?4, ?5)", params![message_id, epoch, owner_ref, now.to_rfc3339(), lease_expires_at.to_rfc3339()])?;
        tx.execute(
            "UPDATE custody_inbox SET state = 'processing', updated_at = ?2 WHERE message_id = ?1",
            params![message_id, now.to_rfc3339()],
        )?;
        record_custody_event(
            &tx,
            message_id,
            "receiver",
            "processing",
            &digest(format!("{owner_ref}\n{epoch}").as_bytes()),
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(InboxClaim {
            message_id: message_id.to_owned(),
            epoch: positive_u64("inbox claim epoch", epoch)?,
            owner_ref: owner_ref.to_owned(),
            expires_at: lease_expires_at,
        })
    }

    #[allow(clippy::too_many_lines)] // One transaction visibly contains authority, effect, and receipt.
    pub(crate) fn apply_custody_effect<F>(
        &self,
        claim: &InboxClaim,
        authority: &InboxAuthority,
        effect_digest: &str,
        apply: F,
    ) -> WorkLedgerResult<ProcessedReceipt>
    where
        F: FnOnce(&Transaction<'_>) -> WorkLedgerResult<()>,
    {
        validate_digest("custody effect", effect_digest)?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (
            identity_json,
            target_machine,
            target_incarnation,
            rebind_epoch,
            transfer_digest,
            expected_authority,
            state,
        ): (Vec<u8>, String, String, i64, String, String, String) = tx.query_row(
            "SELECT identity_json, target_machine_ref, target_incarnation_ref, rebind_epoch,
                    transfer_digest, authority_digest, state
               FROM custody_inbox WHERE message_id = ?1",
            [&claim.message_id],
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
        )?;
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        envelope.validate()?;
        if state == "processed" {
            let (stored_effect, stored_receipt_digest, stored_receipt): (String, String, Vec<u8>) =
                tx.query_row(
                    "SELECT effect.effect_digest, inbox.processed_receipt_digest,
                        inbox.processed_receipt_json
                   FROM custody_effects effect
                   JOIN custody_inbox inbox ON inbox.message_id = effect.message_id
                  WHERE effect.message_id = ?1",
                    [&claim.message_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )?;
            if stored_effect != effect_digest {
                return Err(WorkLedgerError::Refused(
                    "custody message was processed with a different effect".to_owned(),
                ));
            }
            let receipt: ProcessedReceipt =
                serde_json::from_slice(&stored_receipt).map_err(|_| {
                    WorkLedgerError::Refused("stored processed receipt is invalid".to_owned())
                })?;
            super::validate_processed_receipt(&receipt)?;
            if receipt.receipt_digest != stored_receipt_digest
                || receipt.message_id != envelope.message_id
                || receipt.identity_digest != envelope.identity_digest
                || receipt.workstream_revision != envelope.workstream_revision
                || receipt.effect_digest != stored_effect
                || receipt.target_machine_ref != target_machine
                || receipt.target_incarnation_ref != target_incarnation
                || receipt.rebind_epoch != positive_u64("inbox rebind epoch", rebind_epoch)?
                || receipt.transfer_digest != transfer_digest
                || receipt.authority_digest != expected_authority
            {
                return Err(WorkLedgerError::Refused(
                    "stored processed receipt does not match custody state".to_owned(),
                ));
            }
            return Ok(receipt);
        }
        verify_inbox_claim(&tx, claim, Utc::now())?;
        if let Some(prior) = envelope.relation.prior_message_id.as_deref() {
            let prior_state: Option<String> = tx
                .query_row(
                    "SELECT state FROM custody_inbox WHERE message_id = ?1",
                    [prior],
                    |row| row.get(0),
                )
                .optional()?;
            if prior_state.as_deref() != Some("processed") {
                return Err(WorkLedgerError::Refused(
                    "custody correction or followup arrived before its processed predecessor"
                        .to_owned(),
                ));
            }
        }
        if state != "processing"
            || authority.workstream_revision != envelope.workstream_revision
            || authority.target_incarnation_ref != target_incarnation
            || authority.authority_digest != expected_authority
        {
            return Err(WorkLedgerError::Refused(
                "live inbox authority no longer matches the message".to_owned(),
            ));
        }
        apply(&tx)?;
        let receipt = processed_receipt(
            &envelope,
            effect_digest,
            &target_machine,
            &target_incarnation,
            positive_u64("inbox rebind epoch", rebind_epoch)?,
            &transfer_digest,
            &expected_authority,
            claim,
        )?;
        let receipt_json = serde_json::to_vec(&receipt).map_err(|_| {
            WorkLedgerError::Refused("processed receipt cannot be serialized".to_owned())
        })?;
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO custody_effects (message_id, effect_digest, applied_at) VALUES (?1, ?2, ?3)", params![claim.message_id, effect_digest, now])?;
        tx.execute(
            "UPDATE custody_inbox SET state = 'processed', effect_digest = ?2,
                    processed_receipt_digest = ?3, processed_receipt_json = ?4,
                    processed_at = ?5, updated_at = ?5
                    WHERE message_id = ?1 AND state = 'processing'",
            params![
                claim.message_id,
                effect_digest,
                receipt.receipt_digest,
                receipt_json,
                now
            ],
        )?;
        release_inbox_claim(&tx, claim, &now)?;
        record_custody_event(
            &tx,
            &claim.message_id,
            "receiver",
            "processed",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn apply_remote_custody_control(
        &self,
        authenticated: &AuthenticatedCustodyControl,
    ) -> WorkLedgerResult<CustodyControlReceipt> {
        validate_control(&authenticated.control)?;
        validate_digest(
            "custody control transport witness",
            &authenticated.transport_auth_digest,
        )?;
        let control = &authenticated.control;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (identity_json, identity_digest, epoch, state): (Vec<u8>, String, i64, String) = tx
            .query_row(
                "SELECT identity_json, identity_digest, rebind_epoch, state
                   FROM custody_inbox WHERE message_id = ?1",
                [&control.message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        if authenticated.authenticated_source_machine_ref != envelope.source_machine_ref
            || identity_digest != control.identity_digest
            || epoch != sqlite_i64("control rebind epoch", control.expected_rebind_epoch)?
            || envelope.workstream_revision != control.workstream_revision
        {
            return Err(WorkLedgerError::Refused(
                "custody control does not match the received authority".to_owned(),
            ));
        }
        let terminal = control_kind_str(control.kind);
        if state == terminal {
            let persisted = load_control(&tx, &control.control_id)?;
            let (persisted_state, persisted_receipt): (String, String) = tx.query_row(
                "SELECT state, receipt_digest FROM custody_controls WHERE control_id = ?1",
                [&control.control_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let expected = control_receipt(control)?;
            if persisted != *control
                || persisted_state != "acknowledged"
                || persisted_receipt != expected.receipt_digest
            {
                return Err(WorkLedgerError::Refused(
                    "terminal custody control is not an exact replay".to_owned(),
                ));
            }
            return Ok(expected);
        }
        if state != "received" {
            return Err(WorkLedgerError::Refused(
                "custody control arrived after processing began; append a correction instead"
                    .to_owned(),
            ));
        }
        let receipt = control_receipt(control)?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO custody_controls
             (control_id, message_id, identity_digest, kind, successor_message_id,
              expected_rebind_epoch, workstream_revision, authority_digest, control_digest,
              state, receipt_digest, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'acknowledged', ?10, ?11, ?11)",
            params![
                control.control_id,
                control.message_id,
                control.identity_digest,
                terminal,
                control.successor_message_id,
                control.expected_rebind_epoch,
                control.workstream_revision,
                control.authority_digest,
                control.control_digest,
                receipt.receipt_digest,
                now
            ],
        )?;
        tx.execute(
            "UPDATE custody_inbox SET state = ?2, updated_at = ?3 WHERE message_id = ?1 AND state = 'received'",
            params![control.message_id, terminal, now],
        )?;
        record_custody_event(
            &tx,
            &control.message_id,
            "receiver",
            terminal,
            &control.control_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(receipt)
    }
}
