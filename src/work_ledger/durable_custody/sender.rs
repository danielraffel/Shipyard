use super::{
    AuthenticatedCustodyControlReceipt, AuthenticatedCustodyReceipt, AuthenticatedProcessedReceipt,
    CustodyControl, CustodyControlKind, CustodyEnvelope, CustodyKind, CustodyTransfer, DateTime,
    OptionalExtension, SenderClaim, TransactionBehavior, Utc, WorkLedger, WorkLedgerError,
    WorkLedgerResult, control_kind_str, control_receipt, digest, load_control, opaque_ref, params,
    positive_u64, receipt_from_transfer, record_custody_event, relation_prior_state,
    release_expired_claims, release_sender_claim, sqlite_i64, transfer_digest, transfer_from_tx,
    validate_control_receipt, validate_digest, validate_lease, validate_opaque_ref,
    validate_processed_receipt, validate_receipt, validate_target, verify_sender_claim,
};

impl WorkLedger {
    pub(crate) fn custody_status(&self) -> WorkLedgerResult<super::CustodyStatus> {
        let connection = self.custody_read_connection()?;
        let count = |table: &str, state: &str| -> WorkLedgerResult<u64> {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE state = ?1");
            let value: i64 = connection.query_row(&sql, [state], |row| row.get(0))?;
            u64::try_from(value).map_err(|_| {
                WorkLedgerError::Refused("custody status count is out of range".to_owned())
            })
        };
        let pending_controls: i64 = connection.query_row(
            "SELECT COUNT(*) FROM custody_controls WHERE state = 'pending'",
            [],
            |row| row.get(0),
        )?;
        let pending_rebinds: i64 = connection.query_row(
            "SELECT COUNT(*) FROM custody_successor_rebinds
              WHERE side = 'sender' AND state = 'prepared'",
            [],
            |row| row.get(0),
        )?;
        Ok(super::CustodyStatus {
            outgoing_pending: count("custody_outbox", "pending")?,
            outgoing_claimed: count("custody_outbox", "claimed")?,
            outgoing_accepted: count("custody_outbox", "custody_accepted")?,
            outgoing_processed: count("custody_outbox", "processed")?,
            outgoing_cancelled: count("custody_outbox", "cancelled")?,
            outgoing_superseded: count("custody_outbox", "superseded")?,
            incoming_received: count("custody_inbox", "received")?,
            incoming_processing: count("custody_inbox", "processing")?,
            incoming_processed: count("custody_inbox", "processed")?,
            incoming_cancelled: count("custody_inbox", "cancelled")?,
            incoming_superseded: count("custody_inbox", "superseded")?,
            pending_controls: u64::try_from(pending_controls).map_err(|_| {
                WorkLedgerError::Refused("custody status count is out of range".to_owned())
            })?,
            pending_rebinds: u64::try_from(pending_rebinds).map_err(|_| {
                WorkLedgerError::Refused("custody status count is out of range".to_owned())
            })?,
        })
    }

    pub(crate) fn custody_send_candidates(&self, limit: usize) -> WorkLedgerResult<Vec<String>> {
        let connection = self.custody_read_connection()?;
        let mut statement = connection.prepare(
            "SELECT message_id FROM custody_outbox
              WHERE state IN ('pending', 'claimed') ORDER BY updated_at, message_id LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            row.get::<_, String>(0)
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    pub(crate) fn pending_custody_controls(
        &self,
        limit: usize,
    ) -> WorkLedgerResult<Vec<(CustodyControl, String)>> {
        let connection = self.custody_read_connection()?;
        let mut statement = connection.prepare(
            "SELECT control.control_id, rebind.target_machine_ref
               FROM custody_controls control
               JOIN custody_outbox message ON message.message_id = control.message_id
               JOIN custody_rebinds rebind ON rebind.message_id = message.message_id
                                      AND rebind.epoch = message.active_rebind_epoch
              WHERE control.state = 'pending'
              ORDER BY control.updated_at, control.control_id LIMIT ?1",
        )?;
        let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let identities = rows.collect::<Result<Vec<_>, _>>()?;
        identities
            .into_iter()
            .map(|(control_id, machine)| {
                load_control(&connection, &control_id).map(|control| (control, machine))
            })
            .collect()
    }

    #[allow(clippy::too_many_lines)] // Staging validates source, replay, target, and event atomically.
    pub(crate) fn stage_cross_machine_custody(
        &self,
        envelope: &CustodyEnvelope,
        target_machine_ref: &str,
        target_incarnation_ref: &str,
        target_route_ref: &str,
        terminal_adapter: &str,
        authority_digest: &str,
    ) -> WorkLedgerResult<()> {
        envelope.validate()?;
        validate_target(
            target_machine_ref,
            target_incarnation_ref,
            target_route_ref,
            terminal_adapter,
            authority_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Vec<u8>, String, i64)> = tx
            .query_row(
                "SELECT identity_json, identity_digest, active_rebind_epoch
                   FROM custody_outbox WHERE message_id = ?1",
                [&envelope.message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((identity_json, identity_digest, epoch)) = existing {
            let target: (String, String, String, String, String) = tx.query_row(
                "SELECT target_machine_ref, target_incarnation_ref, target_route_ref,
                        terminal_adapter, authority_digest
                   FROM custody_rebinds WHERE message_id = ?1 AND epoch = ?2",
                params![envelope.message_id, epoch],
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
            let encoded = serde_json::to_vec(envelope).map_err(|_| {
                WorkLedgerError::Refused("custody envelope cannot be serialized".to_owned())
            })?;
            if identity_json == encoded
                && identity_digest == envelope.identity_digest
                && target
                    == (
                        target_machine_ref.to_owned(),
                        target_incarnation_ref.to_owned(),
                        target_route_ref.to_owned(),
                        terminal_adapter.to_owned(),
                        authority_digest.to_owned(),
                    )
            {
                return Ok(());
            }
            return Err(WorkLedgerError::Refused(
                "custody message ID already names different immutable content or target".to_owned(),
            ));
        }
        let prior_state = relation_prior_state(&tx, &envelope.relation)?;
        if matches!(
            envelope.relation.kind,
            CustodyKind::Correction | CustodyKind::Followup
        ) && prior_state.as_deref() != Some("processed")
        {
            return Err(WorkLedgerError::Refused(
                "corrections and followups require a processed prior message".to_owned(),
            ));
        }
        let wake: Option<(String, i64, i64, String)> = tx.query_row(
            "SELECT work_item_id, work_generation, owner_generation, payload_digest FROM outbox WHERE wake_id = ?1",
            [&envelope.wake_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).optional()?;
        let Some((work_id, work_generation, owner_generation, payload_digest)) = wake else {
            return Err(WorkLedgerError::Refused(
                "custody source wake is missing".to_owned(),
            ));
        };
        if work_id != envelope.work_item_id
            || work_generation != sqlite_i64("work generation", envelope.work_generation)?
            || owner_generation != sqlite_i64("owner generation", envelope.owner_generation)?
            || payload_digest != envelope.content_digest
        {
            return Err(WorkLedgerError::Refused(
                "custody envelope does not match the durable source wake".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let identity_json = serde_json::to_vec(envelope).map_err(|_| {
            WorkLedgerError::Refused("custody envelope cannot be serialized".to_owned())
        })?;
        tx.execute(
            "INSERT INTO custody_outbox
             (message_id, wake_id, identity_json, identity_digest, state, active_rebind_epoch, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', 1, ?5, ?5)",
            params![envelope.message_id, envelope.wake_id, identity_json, envelope.identity_digest, now],
        )?;
        tx.execute(
            "INSERT INTO custody_rebinds
             (message_id, epoch, target_machine_ref, target_incarnation_ref, target_route_ref,
              terminal_adapter, authority_digest, created_at)
             VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                envelope.message_id,
                target_machine_ref,
                target_incarnation_ref,
                target_route_ref,
                terminal_adapter,
                authority_digest,
                now
            ],
        )?;
        record_custody_event(
            &tx,
            &envelope.message_id,
            "sender",
            "staged",
            &envelope.identity_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn claim_custody_send(
        &self,
        message_id: &str,
        owner_ref: &str,
        lease_expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<SenderClaim> {
        validate_opaque_ref("custody message", message_id, "wm")?;
        validate_opaque_ref("custody sender owner", owner_ref, "owner")?;
        let now = Utc::now();
        validate_lease(now, lease_expires_at)?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        release_expired_claims(&tx, "custody_sender_claims", message_id, now)?;
        let state: String = tx.query_row(
            "SELECT state FROM custody_outbox WHERE message_id = ?1",
            [message_id],
            |row| row.get(0),
        )?;
        if state != "pending" && state != "claimed" {
            return Err(WorkLedgerError::Refused(
                "custody message is not sendable".to_owned(),
            ));
        }
        let active: i64 = tx.query_row(
            "SELECT COUNT(*) FROM custody_sender_claims WHERE message_id = ?1 AND state = 'active'",
            [message_id],
            |row| row.get(0),
        )?;
        if active != 0 {
            return Err(WorkLedgerError::Refused(
                "custody message already has an active sender".to_owned(),
            ));
        }
        let epoch: i64 = tx.query_row(
            "SELECT coalesce(max(epoch), 0) + 1 FROM custody_sender_claims WHERE message_id = ?1",
            [message_id],
            |row| row.get(0),
        )?;
        tx.execute(
            "INSERT INTO custody_sender_claims
             (message_id, epoch, owner_ref, state, acquired_at, expires_at)
             VALUES (?1, ?2, ?3, 'active', ?4, ?5)",
            params![
                message_id,
                epoch,
                owner_ref,
                now.to_rfc3339(),
                lease_expires_at.to_rfc3339()
            ],
        )?;
        tx.execute(
            "UPDATE custody_outbox SET state = 'claimed', updated_at = ?2 WHERE message_id = ?1",
            params![message_id, now.to_rfc3339()],
        )?;
        record_custody_event(
            &tx,
            message_id,
            "sender",
            "claimed",
            &digest(format!("{owner_ref}\n{epoch}").as_bytes()),
            &now.to_rfc3339(),
        )?;
        tx.commit()?;
        Ok(SenderClaim {
            message_id: message_id.to_owned(),
            epoch: positive_u64("sender claim epoch", epoch)?,
            owner_ref: owner_ref.to_owned(),
            expires_at: lease_expires_at,
        })
    }

    pub(crate) fn custody_transfer(
        &self,
        claim: &SenderClaim,
    ) -> WorkLedgerResult<CustodyTransfer> {
        let connection = self.custody_read_connection()?;
        let now = Utc::now();
        verify_sender_claim(&connection, claim, now)?;
        let (identity_json, rebind_epoch): (Vec<u8>, i64) = connection.query_row(
            "SELECT identity_json, active_rebind_epoch FROM custody_outbox WHERE message_id = ?1 AND state = 'claimed'",
            [&claim.message_id], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        envelope.validate()?;
        let (machine, incarnation, route, adapter, authority): (String, String, String, String, String) = connection.query_row(
            "SELECT target_machine_ref, target_incarnation_ref, target_route_ref, terminal_adapter, authority_digest
             FROM custody_rebinds WHERE message_id = ?1 AND epoch = ?2",
            params![claim.message_id, rebind_epoch],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
        )?;
        let transfer_digest = transfer_digest(
            &envelope,
            positive_u64("sender rebind epoch", rebind_epoch)?,
            &machine,
            &incarnation,
            &route,
            &adapter,
            &authority,
        )?;
        Ok(CustodyTransfer {
            envelope,
            rebind_epoch: positive_u64("sender rebind epoch", rebind_epoch)?,
            target_machine_ref: machine,
            target_incarnation_ref: incarnation,
            target_route_ref: route,
            terminal_adapter: adapter,
            rebind_authority_digest: authority,
            transfer_digest,
        })
    }

    pub(crate) fn acknowledge_remote_custody(
        &self,
        claim: &SenderClaim,
        authenticated: &AuthenticatedCustodyReceipt,
    ) -> WorkLedgerResult<()> {
        let receipt = &authenticated.receipt;
        validate_receipt(receipt)?;
        validate_digest(
            "custody receipt transport witness",
            &authenticated.transport_auth_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        verify_sender_claim(&tx, claim, Utc::now())?;
        let transfer = transfer_from_tx(&tx, claim)?;
        if authenticated.authenticated_peer_machine_ref != transfer.target_machine_ref {
            return Err(WorkLedgerError::Refused(
                "custody receipt came from a different authenticated target".to_owned(),
            ));
        }
        let expected = receipt_from_transfer(&transfer, None)?;
        if expected != *receipt {
            return Err(WorkLedgerError::Refused(
                "custody receipt does not match the claimed transfer".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "UPDATE custody_outbox SET state = 'custody_accepted', custody_receipt_digest = ?2,
                    custody_transfer_digest = ?3, updated_at = ?4
             WHERE message_id = ?1 AND state = 'claimed'",
            params![
                claim.message_id,
                receipt.receipt_digest,
                receipt.transfer_digest,
                now
            ],
        )?;
        release_sender_claim(&tx, claim, &now)?;
        record_custody_event(
            &tx,
            &claim.message_id,
            "sender",
            "custody_accepted",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn acknowledge_remote_processed(
        &self,
        authenticated: &AuthenticatedProcessedReceipt,
    ) -> WorkLedgerResult<()> {
        let receipt = &authenticated.receipt;
        validate_processed_receipt(receipt)?;
        validate_digest(
            "processed receipt transport witness",
            &authenticated.transport_auth_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (identity_json, state, rebind_epoch, transfer_digest, stored_processed_digest): (
            Vec<u8>,
            String,
            i64,
            Option<String>,
            Option<String>,
        ) = tx.query_row(
            "SELECT identity_json, state, active_rebind_epoch, custody_transfer_digest,
                    processed_receipt_digest
               FROM custody_outbox WHERE message_id = ?1",
            [&receipt.message_id],
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
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        let (machine, incarnation, authority): (String, String, String) = tx.query_row(
            "SELECT target_machine_ref, target_incarnation_ref, authority_digest
               FROM custody_rebinds WHERE message_id = ?1 AND epoch = ?2",
            params![receipt.message_id, rebind_epoch],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if authenticated.authenticated_peer_machine_ref != machine
            || receipt.identity_digest != envelope.identity_digest
            || receipt.workstream_revision != envelope.workstream_revision
            || receipt.target_machine_ref != machine
            || receipt.target_incarnation_ref != incarnation
            || receipt.rebind_epoch != positive_u64("sender rebind epoch", rebind_epoch)?
            || Some(receipt.transfer_digest.clone()) != transfer_digest
            || receipt.authority_digest != authority
        {
            return Err(WorkLedgerError::Refused(
                "processed receipt does not match the retained message".to_owned(),
            ));
        }
        if state == "processed" {
            return if stored_processed_digest.as_deref() == Some(&receipt.receipt_digest) {
                Ok(())
            } else {
                Err(WorkLedgerError::Refused(
                    "processed acknowledgement contradicts the retained receipt".to_owned(),
                ))
            };
        }
        if state != "custody_accepted" {
            return Err(WorkLedgerError::Refused(
                "processed acknowledgement arrived before custody acceptance".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        tx.execute("UPDATE custody_outbox SET state = 'processed', processed_receipt_digest = ?2, updated_at = ?3 WHERE message_id = ?1", params![receipt.message_id, receipt.receipt_digest, now])?;
        record_custody_event(
            &tx,
            &receipt.message_id,
            "sender",
            "processed",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)] // CAS rebind names both old and complete new target authority.
    pub(crate) fn rebind_unprocessed_custody_target(
        &self,
        message_id: &str,
        expected_incarnation: &str,
        target_machine_ref: &str,
        target_incarnation_ref: &str,
        target_route_ref: &str,
        terminal_adapter: &str,
        authority_digest: &str,
    ) -> WorkLedgerResult<u64> {
        validate_target(
            target_machine_ref,
            target_incarnation_ref,
            target_route_ref,
            terminal_adapter,
            authority_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, epoch): (String, i64) = tx.query_row(
            "SELECT state, active_rebind_epoch FROM custody_outbox WHERE message_id = ?1",
            [message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if state != "pending" {
            return Err(WorkLedgerError::Refused(
                "target rebind requires an unclaimed pending message".to_owned(),
            ));
        }
        let current: String = tx.query_row("SELECT target_incarnation_ref FROM custody_rebinds WHERE message_id = ?1 AND epoch = ?2", params![message_id, epoch], |row| row.get(0))?;
        if current != expected_incarnation {
            return Err(WorkLedgerError::Refused(
                "target incarnation changed before rebind".to_owned(),
            ));
        }
        let next = epoch
            .checked_add(1)
            .ok_or_else(|| WorkLedgerError::Refused("custody rebind epoch exhausted".to_owned()))?;
        let now = Utc::now().to_rfc3339();
        tx.execute("INSERT INTO custody_rebinds (message_id, epoch, target_machine_ref, target_incarnation_ref, target_route_ref, terminal_adapter, authority_digest, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![message_id, next, target_machine_ref, target_incarnation_ref, target_route_ref, terminal_adapter, authority_digest, now])?;
        tx.execute("UPDATE custody_outbox SET active_rebind_epoch = ?2, updated_at = ?3 WHERE message_id = ?1", params![message_id, next, now])?;
        record_custody_event(
            &tx,
            message_id,
            "sender",
            "target_rebound",
            authority_digest,
            &now,
        )?;
        tx.commit()?;
        positive_u64("next custody rebind epoch", next)
    }

    #[allow(clippy::too_many_lines)] // One append-only control staging transaction.
    pub(crate) fn prepare_remote_custody_control(
        &self,
        message_id: &str,
        successor_message_id: Option<&str>,
        authority_digest: &str,
    ) -> WorkLedgerResult<CustodyControl> {
        validate_opaque_ref("custody message", message_id, "wm")?;
        validate_digest("custody control authority", authority_digest)?;
        if let Some(successor) = successor_message_id {
            validate_opaque_ref("custody successor", successor, "wm")?;
            if successor == message_id {
                return Err(WorkLedgerError::Refused(
                    "custody message cannot supersede itself".to_owned(),
                ));
            }
        }
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (identity_json, identity_digest, state, epoch): (Vec<u8>, String, String, i64) = tx
            .query_row(
                "SELECT identity_json, identity_digest, state, active_rebind_epoch
                   FROM custody_outbox WHERE message_id = ?1",
                [message_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        if state != "custody_accepted" {
            return Err(WorkLedgerError::Refused(
                "remote custody control requires accepted but unprocessed custody".to_owned(),
            ));
        }
        let pending_successor: i64 = tx.query_row(
            "SELECT COUNT(*) FROM custody_successor_rebinds
              WHERE message_id = ?1 AND side = 'sender' AND state = 'prepared'",
            [message_id],
            |row| row.get(0),
        )?;
        if pending_successor != 0 {
            return Err(WorkLedgerError::Refused(
                "remote custody control conflicts with a prepared successor".to_owned(),
            ));
        }
        let active_authority: String = tx.query_row(
            "SELECT authority_digest FROM custody_rebinds
              WHERE message_id = ?1 AND epoch = ?2",
            params![message_id, epoch],
            |row| row.get(0),
        )?;
        if authority_digest != active_authority {
            return Err(WorkLedgerError::Refused(
                "remote custody control authority is stale".to_owned(),
            ));
        }
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        let kind = if successor_message_id.is_some() {
            CustodyControlKind::Superseded
        } else {
            CustodyControlKind::Cancelled
        };
        let control_digest = digest(
            &serde_json::to_vec(&(
                message_id,
                &identity_digest,
                kind,
                successor_message_id,
                epoch,
                envelope.workstream_revision,
                authority_digest,
            ))
            .map_err(|_| {
                WorkLedgerError::Refused("custody control cannot be serialized".to_owned())
            })?,
        );
        let control_id = opaque_ref("cc", &control_digest);
        let control = CustodyControl {
            control_id,
            message_id: message_id.to_owned(),
            identity_digest,
            kind,
            successor_message_id: successor_message_id.map(str::to_owned),
            expected_rebind_epoch: positive_u64("custody control rebind epoch", epoch)?,
            workstream_revision: envelope.workstream_revision,
            authority_digest: authority_digest.to_owned(),
            control_digest,
        };
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT OR IGNORE INTO custody_controls
             (control_id, message_id, identity_digest, kind, successor_message_id,
              expected_rebind_epoch, workstream_revision, authority_digest, control_digest,
              state, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending', ?10, ?10)",
            params![
                control.control_id,
                control.message_id,
                control.identity_digest,
                control_kind_str(control.kind),
                control.successor_message_id,
                control.expected_rebind_epoch,
                control.workstream_revision,
                control.authority_digest,
                control.control_digest,
                now
            ],
        )?;
        tx.commit()?;
        Ok(control)
    }

    pub(crate) fn acknowledge_remote_custody_control(
        &self,
        authenticated: &AuthenticatedCustodyControlReceipt,
    ) -> WorkLedgerResult<()> {
        let receipt = &authenticated.receipt;
        validate_control_receipt(receipt)?;
        validate_digest(
            "custody control receipt transport witness",
            &authenticated.transport_auth_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let control: CustodyControl = load_control(&tx, &receipt.control_id)?;
        let target_machine: String = tx.query_row(
            "SELECT rebind.target_machine_ref
               FROM custody_outbox message
               JOIN custody_rebinds rebind ON rebind.message_id = message.message_id
                                        AND rebind.epoch = message.active_rebind_epoch
              WHERE message.message_id = ?1",
            [&receipt.message_id],
            |row| row.get(0),
        )?;
        if authenticated.authenticated_peer_machine_ref != target_machine {
            return Err(WorkLedgerError::Refused(
                "custody control receipt came from a different authenticated target".to_owned(),
            ));
        }
        if control_receipt(&control)? != *receipt {
            return Err(WorkLedgerError::Refused(
                "custody control receipt does not match the pending control".to_owned(),
            ));
        }
        let (control_state, stored_receipt, outbox_state): (String, Option<String>, String) = tx
            .query_row(
                "SELECT control.state, control.receipt_digest, message.state
               FROM custody_controls control
               JOIN custody_outbox message ON message.message_id = control.message_id
              WHERE control.control_id = ?1",
                [&receipt.control_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        if control_state == "acknowledged" {
            if stored_receipt.as_deref() == Some(&receipt.receipt_digest)
                && outbox_state == receipt.terminal_state
            {
                return Ok(());
            }
            return Err(WorkLedgerError::Refused(
                "acknowledged custody control does not match durable terminal state".to_owned(),
            ));
        }
        if control_state != "pending" || outbox_state != "custody_accepted" {
            return Err(WorkLedgerError::Refused(
                "custody control receipt arrived outside its pending transition".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        let changed_control = tx.execute(
            "UPDATE custody_controls SET state = 'acknowledged', receipt_digest = ?2, updated_at = ?3
             WHERE control_id = ?1 AND state = 'pending'",
            params![receipt.control_id, receipt.receipt_digest, now],
        )?;
        let changed_message = tx.execute(
            "UPDATE custody_outbox SET state = ?2, updated_at = ?3
             WHERE message_id = ?1 AND state = 'custody_accepted'",
            params![receipt.message_id, receipt.terminal_state, now],
        )?;
        if changed_control != 1 || changed_message != 1 {
            return Err(WorkLedgerError::Refused(
                "custody control receipt lost its terminal transition race".to_owned(),
            ));
        }
        record_custody_event(
            &tx,
            &receipt.message_id,
            "sender",
            &receipt.terminal_state,
            &receipt.control_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }
}
