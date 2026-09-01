use super::{
    AuthenticatedCustodySuccessorRebind, AuthenticatedCustodySuccessorReceipt, CustodyEnvelope,
    CustodySuccessorRebind, CustodySuccessorReceipt, OptionalExtension, TransactionBehavior, Utc,
    WorkLedger, WorkLedgerError, WorkLedgerResult, digest, opaque_ref, params, positive_u64,
    record_custody_event, release_expired_claims, successor_receipt, validate_digest,
    validate_opaque_ref, validate_successor_rebind, validate_successor_receipt,
};
use crate::work_ledger::OwnershipLease;
use chrono::DateTime;

#[derive(Clone, Debug)]
pub(super) struct ActiveInboxBinding {
    pub(super) epoch: u64,
    pub(super) incarnation: String,
    pub(super) route: String,
    pub(super) adapter: String,
    pub(super) authority_digest: String,
    pub(super) transfer_digest: String,
    pub(super) custody_receipt_digest: String,
}

pub(super) fn active_inbox_binding(
    connection: &rusqlite::Connection,
    message_id: &str,
) -> WorkLedgerResult<ActiveInboxBinding> {
    let successor: Option<(Vec<u8>, Vec<u8>)> = connection
        .query_row(
            "SELECT rebind_json, receipt_json FROM custody_successor_rebinds
              WHERE message_id = ?1 AND side = 'receiver' AND state = 'committed'
              ORDER BY authority_epoch DESC LIMIT 1",
            [message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((rebind_json, receipt_json)) = successor {
        let rebind: CustodySuccessorRebind =
            serde_json::from_slice(&rebind_json).map_err(|_| {
                WorkLedgerError::Refused("stored custody successor is invalid".to_owned())
            })?;
        let receipt: CustodySuccessorReceipt =
            serde_json::from_slice(&receipt_json).map_err(|_| {
                WorkLedgerError::Refused("stored custody successor receipt is invalid".to_owned())
            })?;
        validate_successor_rebind(&rebind)?;
        validate_successor_receipt(&receipt)?;
        if successor_receipt(&rebind)? != receipt {
            return Err(WorkLedgerError::Refused(
                "stored custody successor receipt contradicts its rebind".to_owned(),
            ));
        }
        return Ok(ActiveInboxBinding {
            epoch: rebind.new_authority_epoch,
            incarnation: rebind.new_target_incarnation_ref,
            route: rebind.new_target_route_ref,
            adapter: rebind.terminal_adapter,
            authority_digest: rebind.new_authority_digest,
            transfer_digest: rebind.rebind_digest,
            custody_receipt_digest: receipt.receipt_digest,
        });
    }
    let (epoch, incarnation, route, adapter, authority, transfer, receipt): (
        i64,
        String,
        String,
        String,
        String,
        String,
        String,
    ) = connection.query_row(
        "SELECT rebind_epoch, target_incarnation_ref, target_route_ref, terminal_adapter,
                authority_digest, transfer_digest, custody_receipt_digest
           FROM custody_inbox WHERE message_id = ?1",
        [message_id],
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
    Ok(ActiveInboxBinding {
        epoch: positive_u64("active inbox authority epoch", epoch)?,
        incarnation,
        route,
        adapter,
        authority_digest: authority,
        transfer_digest: transfer,
        custody_receipt_digest: receipt,
    })
}

fn receiver_abort_candidate(
    tx: &rusqlite::Transaction<'_>,
    rebind: &CustodySuccessorRebind,
    expected: &[u8],
) -> WorkLedgerResult<(Vec<u8>, String)> {
    let existing: Option<(Vec<u8>, String)> = tx
        .query_row(
            "SELECT rebind_json, state FROM custody_successor_rebinds
              WHERE rebind_id = ?1 AND side = 'receiver'",
            [&rebind.rebind_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some(existing) = existing {
        return Ok(existing);
    }
    let identity_digest: String = tx.query_row(
        "SELECT identity_digest FROM custody_inbox WHERE message_id = ?1",
        [&rebind.message_id],
        |row| row.get(0),
    )?;
    let active = active_inbox_binding(tx, &rebind.message_id)?;
    if identity_digest != rebind.identity_digest
        || active.epoch != rebind.old_authority_epoch
        || active.incarnation != rebind.old_target_incarnation_ref
        || active.transfer_digest != rebind.old_transfer_digest
        || active.custody_receipt_digest != rebind.old_custody_receipt_digest
    {
        return Err(WorkLedgerError::Refused(
            "absent custody successor abort does not match retained custody".to_owned(),
        ));
    }
    let receipt = successor_receipt(rebind)?;
    let receipt_json = serde_json::to_vec(&receipt).map_err(|_| {
        WorkLedgerError::Refused("custody successor abort receipt cannot be serialized".to_owned())
    })?;
    let now = Utc::now().to_rfc3339();
    tx.execute(
        "INSERT INTO custody_successor_rebinds
         (rebind_id, message_id, side, rebind_json, rebind_digest, authority_epoch,
          state, receipt_json, receipt_digest, created_at, updated_at)
         VALUES (?1, ?2, 'receiver', ?3, ?4, ?5, 'prepared', ?6, ?7, ?8, ?8)",
        params![
            rebind.rebind_id,
            rebind.message_id,
            expected,
            rebind.rebind_digest,
            i64::try_from(rebind.new_authority_epoch).map_err(|_| {
                WorkLedgerError::Refused("custody successor abort epoch is out of range".to_owned())
            })?,
            receipt_json,
            receipt.receipt_digest,
            now,
        ],
    )?;
    Ok((expected.to_vec(), "prepared".to_owned()))
}

impl WorkLedger {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_custody_successor_rebind_with_holder(
        &self,
        message_id: &str,
        expected_old_incarnation_ref: &str,
        new_target_incarnation_ref: &str,
        new_target_route_ref: &str,
        terminal_adapter: &str,
        new_authority_digest: &str,
        ownership_id: &str,
        expected_lease_generation: u64,
        holder_bytes: &[u8],
    ) -> WorkLedgerResult<CustodySuccessorRebind> {
        let lease = self.active_ownership_lease_with_material(
            ownership_id,
            holder_bytes,
            expected_lease_generation,
        )?;
        self.prepare_custody_successor_rebind(
            message_id,
            expected_old_incarnation_ref,
            new_target_incarnation_ref,
            new_target_route_ref,
            terminal_adapter,
            new_authority_digest,
            &lease,
        )
    }

    pub(crate) fn pending_custody_successor_rebinds(
        &self,
        limit: usize,
    ) -> WorkLedgerResult<Vec<CustodySuccessorRebind>> {
        let connection = self.custody_read_connection()?;
        let mut statement = connection.prepare(
            "SELECT rebind_json FROM custody_successor_rebinds
              WHERE side = 'sender'
                AND (state = 'acknowledged'
                     OR (state = 'prepared' AND
                         json_extract(CAST(rebind_json AS TEXT), '$.ownership_lease_expires_at') > ?1))
              ORDER BY updated_at, rebind_id LIMIT ?2",
        )?;
        let rows = statement.query_map(
            params![
                Utc::now().to_rfc3339(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        rows.map(|row| {
            let encoded = row?;
            let rebind = serde_json::from_slice(&encoded).map_err(|_| {
                WorkLedgerError::Refused("stored custody successor is invalid".to_owned())
            })?;
            validate_successor_rebind(&rebind)?;
            Ok(rebind)
        })
        .collect()
    }

    pub(crate) fn expired_prepared_custody_successor_rebinds(
        &self,
        limit: usize,
    ) -> WorkLedgerResult<Vec<CustodySuccessorRebind>> {
        let connection = self.custody_read_connection()?;
        let mut statement = connection.prepare(
            "SELECT rebind_json FROM custody_successor_rebinds
              WHERE side = 'sender' AND state = 'prepared'
                AND json_extract(CAST(rebind_json AS TEXT), '$.ownership_lease_expires_at') <= ?1
              ORDER BY updated_at, rebind_id LIMIT ?2",
        )?;
        let now = Utc::now().to_rfc3339();
        let rows = statement.query_map(
            params![now, i64::try_from(limit).unwrap_or(i64::MAX)],
            |row| row.get::<_, Vec<u8>>(0),
        )?;
        rows.map(|row| {
            let encoded = row?;
            let rebind = serde_json::from_slice(&encoded).map_err(|_| {
                WorkLedgerError::Refused("stored custody successor is invalid".to_owned())
            })?;
            validate_successor_rebind(&rebind)?;
            Ok(rebind)
        })
        .collect()
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)] // One fenced source transaction.
    pub(crate) fn prepare_custody_successor_rebind(
        &self,
        message_id: &str,
        expected_old_incarnation_ref: &str,
        new_target_incarnation_ref: &str,
        new_target_route_ref: &str,
        terminal_adapter: &str,
        new_authority_digest: &str,
        successor_lease: &OwnershipLease,
    ) -> WorkLedgerResult<CustodySuccessorRebind> {
        self.prepare_custody_successor_rebind_with_proof(
            message_id,
            expected_old_incarnation_ref,
            new_target_incarnation_ref,
            new_target_route_ref,
            terminal_adapter,
            new_authority_digest,
            &successor_lease.proof_digest,
            Some(successor_lease),
            None,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_custody_successor_rebind_for_test(
        &self,
        message_id: &str,
        expected_old_incarnation_ref: &str,
        new_target_incarnation_ref: &str,
        new_target_route_ref: &str,
        terminal_adapter: &str,
        new_authority_digest: &str,
        successor_proof_digest: &str,
        lease_expires_at: DateTime<Utc>,
    ) -> WorkLedgerResult<CustodySuccessorRebind> {
        self.prepare_custody_successor_rebind_with_proof(
            message_id,
            expected_old_incarnation_ref,
            new_target_incarnation_ref,
            new_target_route_ref,
            terminal_adapter,
            new_authority_digest,
            successor_proof_digest,
            None,
            Some(lease_expires_at),
        )
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    fn prepare_custody_successor_rebind_with_proof(
        &self,
        message_id: &str,
        expected_old_incarnation_ref: &str,
        new_target_incarnation_ref: &str,
        new_target_route_ref: &str,
        terminal_adapter: &str,
        new_authority_digest: &str,
        successor_proof_digest: &str,
        successor_lease: Option<&OwnershipLease>,
        test_lease_expires_at: Option<DateTime<Utc>>,
    ) -> WorkLedgerResult<CustodySuccessorRebind> {
        validate_digest("custody successor authority", new_authority_digest)?;
        validate_digest("custody successor proof", successor_proof_digest)?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (identity_json, identity_digest, state, epoch, custody_receipt, transfer_digest): (
            Vec<u8>,
            String,
            String,
            i64,
            Option<String>,
            Option<String>,
        ) = tx.query_row(
            "SELECT identity_json, identity_digest, state, active_rebind_epoch,
                    custody_receipt_digest, custody_transfer_digest
               FROM custody_outbox WHERE message_id = ?1",
            [message_id],
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
        )?;
        if state != "custody_accepted" {
            return Err(WorkLedgerError::Refused(
                "custody successor requires accepted unprocessed custody".to_owned(),
            ));
        }
        let pending_control: i64 = tx.query_row(
            "SELECT COUNT(*) FROM custody_controls WHERE message_id = ?1 AND state = 'pending'",
            [message_id],
            |row| row.get(0),
        )?;
        if pending_control != 0 {
            return Err(WorkLedgerError::Refused(
                "custody successor conflicts with a pending terminal control".to_owned(),
            ));
        }
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        if let Some(lease) = successor_lease {
            super::super::ownership_lease::validate_active_successor_lease_for_custody(
                &tx,
                lease,
                &envelope.work_item_id,
                envelope.work_generation,
                envelope.owner_generation,
                &envelope.workstream_handle,
            )?;
        }
        let (target_machine, old_incarnation): (String, String) = tx.query_row(
            "SELECT target_machine_ref, target_incarnation_ref FROM custody_rebinds
              WHERE message_id = ?1 AND epoch = ?2",
            params![message_id, epoch],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if old_incarnation != expected_old_incarnation_ref {
            return Err(WorkLedgerError::Refused(
                "custody successor old incarnation is stale".to_owned(),
            ));
        }
        let old_epoch = positive_u64("custody successor old epoch", epoch)?;
        let new_epoch = old_epoch.checked_add(1).ok_or_else(|| {
            WorkLedgerError::Refused("custody successor authority epoch exhausted".to_owned())
        })?;
        let custody_receipt = custody_receipt.ok_or_else(|| {
            WorkLedgerError::Refused("accepted custody receipt is missing".to_owned())
        })?;
        let transfer_digest = transfer_digest.ok_or_else(|| {
            WorkLedgerError::Refused("accepted custody transfer is missing".to_owned())
        })?;
        let (
            ownership_lease_id,
            ownership_lease_generation,
            ownership_lease_expires_at,
            ownership_root_uuid,
            repository_provider,
            repository_id,
            repository,
            pull_request,
            exact_head,
            lease_workstream_handle,
            successor_holder_ref,
            successor_session_incarnation_ref,
        ) = successor_lease.map_or_else(
            || {
                (
                    opaque_ref("ol", "test-only custody successor lease"),
                    1,
                    test_lease_expires_at
                        .unwrap_or_else(|| Utc::now() + chrono::Duration::minutes(5)),
                    "00000000-0000-0000-0000-000000000001".to_owned(),
                    "github.com".to_owned(),
                    "R_test".to_owned(),
                    "test/repository".to_owned(),
                    1,
                    "0".repeat(40),
                    envelope.workstream_handle.clone(),
                    opaque_ref("owner", "test-only custody successor holder"),
                    opaque_ref("incarnation", "test-only custody successor holder session"),
                )
            },
            |lease| {
                (
                    lease.lease_id.clone(),
                    lease.lease_generation,
                    lease.expires_at,
                    lease.fence.root_uuid.clone(),
                    lease.fence.repository_provider.clone(),
                    lease.fence.repository_id.clone(),
                    lease.fence.repository.clone(),
                    lease.fence.pull_request,
                    lease.fence.exact_head.clone(),
                    lease.fence.workstream_handle.clone(),
                    lease.holder.holder_ref.clone(),
                    lease.holder.incarnation_ref.clone(),
                )
            },
        );
        let mut rebind = CustodySuccessorRebind {
            rebind_id: String::new(),
            message_id: message_id.to_owned(),
            identity_digest,
            workstream_revision: envelope.workstream_revision,
            source_machine_ref: envelope.source_machine_ref,
            target_machine_ref: target_machine,
            old_target_incarnation_ref: old_incarnation,
            new_target_incarnation_ref: new_target_incarnation_ref.to_owned(),
            old_authority_epoch: old_epoch,
            new_authority_epoch: new_epoch,
            old_transfer_digest: transfer_digest,
            old_custody_receipt_digest: custody_receipt,
            new_target_route_ref: new_target_route_ref.to_owned(),
            terminal_adapter: terminal_adapter.to_owned(),
            new_authority_digest: new_authority_digest.to_owned(),
            ownership_lease_id,
            ownership_lease_generation,
            ownership_lease_expires_at,
            ownership_root_uuid,
            repository_provider,
            repository_id,
            repository,
            pull_request,
            exact_head,
            workstream_handle: lease_workstream_handle,
            successor_holder_ref,
            successor_session_incarnation_ref,
            successor_proof_digest: successor_proof_digest.to_owned(),
            rebind_digest: String::new(),
        };
        rebind.rebind_digest = digest(
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
        rebind.rebind_id = opaque_ref("cr", &rebind.rebind_digest);
        validate_successor_rebind(&rebind)?;
        let encoded = serde_json::to_vec(&rebind).map_err(|_| {
            WorkLedgerError::Refused("custody successor cannot be serialized".to_owned())
        })?;
        let existing: Option<Vec<u8>> = tx
            .query_row(
                "SELECT rebind_json FROM custody_successor_rebinds
                  WHERE message_id = ?1 AND side = 'sender' AND state = 'prepared'",
                [message_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            return if existing == encoded {
                Ok(rebind)
            } else {
                Err(WorkLedgerError::Refused(
                    "custody successor already has a contradictory prepared migration".to_owned(),
                ))
            };
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO custody_successor_rebinds
             (rebind_id, message_id, side, rebind_json, rebind_digest, authority_epoch,
              state, created_at, updated_at)
             VALUES (?1, ?2, 'sender', ?3, ?4, ?5, 'prepared', ?6, ?6)",
            params![
                rebind.rebind_id,
                message_id,
                encoded,
                rebind.rebind_digest,
                i64::try_from(rebind.new_authority_epoch).map_err(|_| {
                    WorkLedgerError::Refused(
                        "custody successor authority epoch is out of range".to_owned(),
                    )
                })?,
                now
            ],
        )?;
        record_custody_event(
            &tx,
            message_id,
            "sender",
            "successor_prepared",
            &rebind.rebind_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(rebind)
    }

    #[allow(clippy::too_many_lines)] // One fenced destination transaction.
    pub(crate) fn accept_custody_successor_rebind(
        &self,
        authenticated: &AuthenticatedCustodySuccessorRebind,
        local_machine_ref: &str,
        live_successor_incarnation_ref: &str,
        live_route_ref: &str,
        live_terminal_adapter: &str,
        live_authority_digest: &str,
    ) -> WorkLedgerResult<CustodySuccessorReceipt> {
        let rebind = &authenticated.rebind;
        validate_successor_rebind(rebind)?;
        validate_digest(
            "custody successor transport",
            &authenticated.transport_auth_digest,
        )?;
        if authenticated.authenticated_source_machine_ref != rebind.source_machine_ref
            || rebind.target_machine_ref != local_machine_ref
        {
            return Err(WorkLedgerError::Refused(
                "custody successor does not match authenticated live endpoints".to_owned(),
            ));
        }
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<(Vec<u8>, Vec<u8>, String)> = tx
            .query_row(
                "SELECT rebind_json, receipt_json, state FROM custody_successor_rebinds
                  WHERE rebind_id = ?1 AND side = 'receiver'",
                [&rebind.rebind_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((stored_rebind, stored_receipt, state)) = existing {
            let expected_rebind = serde_json::to_vec(rebind).map_err(|_| {
                WorkLedgerError::Refused("custody successor cannot be serialized".to_owned())
            })?;
            let receipt: CustodySuccessorReceipt = serde_json::from_slice(&stored_receipt)
                .map_err(|_| {
                    WorkLedgerError::Refused(
                        "stored custody successor receipt is invalid".to_owned(),
                    )
                })?;
            if stored_rebind == expected_rebind
                && successor_receipt(rebind)? == receipt
                && state != "aborted"
            {
                return Ok(receipt);
            }
            if stored_rebind == expected_rebind && state == "aborted" {
                return Err(WorkLedgerError::Refused(
                    "custody successor was durably aborted before delayed preparation".to_owned(),
                ));
            }
            return Err(WorkLedgerError::Refused(
                "custody successor replay is contradictory".to_owned(),
            ));
        }
        if rebind.new_target_incarnation_ref != live_successor_incarnation_ref
            || rebind.new_target_route_ref != live_route_ref
            || rebind.terminal_adapter != live_terminal_adapter
            || rebind.new_authority_digest != live_authority_digest
        {
            return Err(WorkLedgerError::Refused(
                "new custody successor does not match the live destination endpoint".to_owned(),
            ));
        }
        if rebind.ownership_lease_expires_at <= Utc::now() {
            return Err(WorkLedgerError::Refused(
                "custody successor lease expired before receiver preparation".to_owned(),
            ));
        }
        let (identity_json, identity_digest, state): (Vec<u8>, String, String) = tx.query_row(
            "SELECT identity_json, identity_digest, state FROM custody_inbox WHERE message_id = ?1",
            [&rebind.message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        release_expired_claims(&tx, "custody_inbox_claims", &rebind.message_id, Utc::now())?;
        if state != "received" && state != "processing" {
            return Err(WorkLedgerError::Refused(
                "custody successor requires unclaimed received custody".to_owned(),
            ));
        }
        let claims: i64 = tx.query_row(
            "SELECT COUNT(*) FROM custody_inbox_claims
              WHERE message_id = ?1 AND state = 'active'",
            [&rebind.message_id],
            |row| row.get(0),
        )?;
        if claims != 0 {
            return Err(WorkLedgerError::Refused(
                "custody successor lost the consumer claim race".to_owned(),
            ));
        }
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("stored custody envelope is invalid".to_owned())
        })?;
        let active = active_inbox_binding(&tx, &rebind.message_id)?;
        if identity_digest != rebind.identity_digest
            || envelope.workstream_revision != rebind.workstream_revision
            || envelope.workstream_handle != rebind.workstream_handle
            || envelope.source_machine_ref != rebind.source_machine_ref
            || active.epoch != rebind.old_authority_epoch
            || active.incarnation != rebind.old_target_incarnation_ref
            || active.transfer_digest != rebind.old_transfer_digest
            || active.custody_receipt_digest != rebind.old_custody_receipt_digest
        {
            return Err(WorkLedgerError::Refused(
                "custody successor no longer matches retained destination custody".to_owned(),
            ));
        }
        let receipt = successor_receipt(rebind)?;
        let rebind_json = serde_json::to_vec(rebind).map_err(|_| {
            WorkLedgerError::Refused("custody successor cannot be serialized".to_owned())
        })?;
        let receipt_json = serde_json::to_vec(&receipt).map_err(|_| {
            WorkLedgerError::Refused("custody successor receipt cannot be serialized".to_owned())
        })?;
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO custody_successor_rebinds
             (rebind_id, message_id, side, rebind_json, rebind_digest, authority_epoch, state,
              receipt_json, receipt_digest, created_at, updated_at)
             VALUES (?1, ?2, 'receiver', ?3, ?4, ?5, 'prepared', ?6, ?7, ?8, ?8)",
            params![
                rebind.rebind_id,
                rebind.message_id,
                rebind_json,
                rebind.rebind_digest,
                i64::try_from(rebind.new_authority_epoch).map_err(|_| {
                    WorkLedgerError::Refused(
                        "custody successor authority epoch is out of range".to_owned(),
                    )
                })?,
                receipt_json,
                receipt.receipt_digest,
                now
            ],
        )?;
        tx.execute(
            "UPDATE custody_inbox SET state = 'received', updated_at = ?2
              WHERE message_id = ?1 AND state = 'processing'",
            params![rebind.message_id, now],
        )?;
        record_custody_event(
            &tx,
            &rebind.message_id,
            "receiver",
            "successor_receiver_prepared",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(receipt)
    }

    pub(crate) fn abort_expired_custody_successor_rebind(
        &self,
        authenticated: &AuthenticatedCustodySuccessorRebind,
        local_machine_ref: &str,
    ) -> WorkLedgerResult<String> {
        let rebind = &authenticated.rebind;
        validate_successor_rebind(rebind)?;
        validate_digest(
            "custody successor abort transport",
            &authenticated.transport_auth_digest,
        )?;
        if authenticated.authenticated_source_machine_ref != rebind.source_machine_ref
            || rebind.target_machine_ref != local_machine_ref
        {
            return Err(WorkLedgerError::Refused(
                "custody successor abort does not match authenticated endpoints".to_owned(),
            ));
        }
        if rebind.ownership_lease_expires_at > Utc::now() {
            return Err(WorkLedgerError::Refused(
                "live custody successor cannot be aborted".to_owned(),
            ));
        }
        let expected = serde_json::to_vec(rebind).map_err(|_| {
            WorkLedgerError::Refused("custody successor cannot be serialized".to_owned())
        })?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (stored, state) = receiver_abort_candidate(&tx, rebind, &expected)?;
        if stored != expected {
            return Err(WorkLedgerError::Refused(
                "custody successor abort is contradictory".to_owned(),
            ));
        }
        if state == "aborted" {
            return Ok(rebind.rebind_digest.clone());
        }
        if state != "prepared" {
            return Err(WorkLedgerError::Refused(
                "committed custody successor cannot be aborted".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        if tx.execute(
            "UPDATE custody_successor_rebinds SET state = 'aborted', updated_at = ?2
              WHERE rebind_id = ?1 AND side = 'receiver' AND state = 'prepared'",
            params![rebind.rebind_id, now],
        )? != 1
        {
            return Err(WorkLedgerError::Refused(
                "custody successor changed during authenticated abort".to_owned(),
            ));
        }
        record_custody_event(
            &tx,
            &rebind.message_id,
            "receiver",
            "successor_aborted",
            &rebind.rebind_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(rebind.rebind_digest.clone())
    }

    pub(crate) fn mark_custody_successor_aborted(
        &self,
        rebind_id: &str,
        rebind_digest: &str,
    ) -> WorkLedgerResult<()> {
        validate_opaque_ref("custody successor rebind", rebind_id, "cr")?;
        validate_digest("custody successor abort", rebind_digest)?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (message_id, state, stored_digest, encoded): (String, String, String, Vec<u8>) = tx
            .query_row(
                "SELECT message_id, state, rebind_digest, rebind_json
                   FROM custody_successor_rebinds
                  WHERE rebind_id = ?1 AND side = 'sender'",
                [rebind_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )?;
        let rebind: CustodySuccessorRebind = serde_json::from_slice(&encoded).map_err(|_| {
            WorkLedgerError::Refused("stored custody successor is invalid".to_owned())
        })?;
        if stored_digest != rebind_digest || rebind.ownership_lease_expires_at > Utc::now() {
            return Err(WorkLedgerError::Refused(
                "custody successor abort acknowledgement is stale".to_owned(),
            ));
        }
        if state == "aborted" {
            return Ok(());
        }
        if state != "prepared" {
            return Err(WorkLedgerError::Refused(
                "custody successor abort arrived after acknowledgement".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        if tx.execute(
            "UPDATE custody_successor_rebinds SET state = 'aborted', updated_at = ?2
              WHERE rebind_id = ?1 AND side = 'sender' AND state = 'prepared'",
            params![rebind_id, now],
        )? != 1
        {
            return Err(WorkLedgerError::Refused(
                "custody successor changed during abort acknowledgement".to_owned(),
            ));
        }
        record_custody_event(
            &tx,
            &message_id,
            "sender",
            "successor_aborted",
            rebind_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Commit a receiver-prepared successor only after the source has reread
    /// and committed the same live lease tuple, then authenticated this receipt.
    pub(crate) fn commit_custody_successor_rebind(
        &self,
        authenticated: &AuthenticatedCustodySuccessorReceipt,
    ) -> WorkLedgerResult<()> {
        let receipt = &authenticated.receipt;
        validate_successor_receipt(receipt)?;
        validate_digest(
            "custody successor final transport",
            &authenticated.transport_auth_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (rebind_json, state, stored_receipt): (Vec<u8>, String, String) = tx.query_row(
            "SELECT rebind_json, state, receipt_digest FROM custody_successor_rebinds
              WHERE rebind_id = ?1 AND side = 'receiver'",
            [&receipt.rebind_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let rebind: CustodySuccessorRebind =
            serde_json::from_slice(&rebind_json).map_err(|_| {
                WorkLedgerError::Refused("stored custody successor is invalid".to_owned())
            })?;
        if authenticated.authenticated_peer_machine_ref != rebind.source_machine_ref
            || successor_receipt(&rebind)? != *receipt
            || stored_receipt != receipt.receipt_digest
        {
            return Err(WorkLedgerError::Refused(
                "custody successor final commit is stale or contradictory".to_owned(),
            ));
        }
        if state == "committed" {
            return Ok(());
        }
        if state != "prepared" {
            return Err(WorkLedgerError::Refused(
                "custody successor receiver is not prepared".to_owned(),
            ));
        }
        release_expired_claims(&tx, "custody_inbox_claims", &rebind.message_id, Utc::now())?;
        let claims: i64 = tx.query_row(
            "SELECT COUNT(*) FROM custody_inbox_claims WHERE message_id = ?1 AND state = 'active'",
            [&rebind.message_id],
            |row| row.get(0),
        )?;
        let active = active_inbox_binding(&tx, &rebind.message_id)?;
        if claims != 0
            || active.epoch != rebind.old_authority_epoch
            || active.incarnation != rebind.old_target_incarnation_ref
            || active.transfer_digest != rebind.old_transfer_digest
            || active.custody_receipt_digest != rebind.old_custody_receipt_digest
        {
            return Err(WorkLedgerError::Refused(
                "custody successor lost its receiver commit race".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        if tx.execute(
            "UPDATE custody_successor_rebinds SET state = 'committed', updated_at = ?2
              WHERE rebind_id = ?1 AND side = 'receiver' AND state = 'prepared'",
            params![rebind.rebind_id, now],
        )? != 1
        {
            return Err(WorkLedgerError::Refused(
                "custody successor receiver changed during final commit".to_owned(),
            ));
        }
        record_custody_event(
            &tx,
            &rebind.message_id,
            "receiver",
            "successor_committed",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // One fenced source acknowledgement transaction.
    pub(crate) fn acknowledge_custody_successor_rebind(
        &self,
        authenticated: &AuthenticatedCustodySuccessorReceipt,
    ) -> WorkLedgerResult<()> {
        self.acknowledge_custody_successor_rebind_with_live_lease(authenticated, true)
    }

    #[cfg(test)]
    pub(crate) fn acknowledge_custody_successor_rebind_for_test(
        &self,
        authenticated: &AuthenticatedCustodySuccessorReceipt,
    ) -> WorkLedgerResult<()> {
        self.acknowledge_custody_successor_rebind_with_live_lease(authenticated, false)
    }

    #[allow(clippy::too_many_lines)]
    fn acknowledge_custody_successor_rebind_with_live_lease(
        &self,
        authenticated: &AuthenticatedCustodySuccessorReceipt,
        require_live_lease: bool,
    ) -> WorkLedgerResult<()> {
        let receipt = &authenticated.receipt;
        validate_successor_receipt(receipt)?;
        validate_digest(
            "custody successor receipt transport",
            &authenticated.transport_auth_digest,
        )?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (rebind_json, state, stored_receipt): (Vec<u8>, String, Option<String>) = tx
            .query_row(
                "SELECT rebind_json, state, receipt_digest FROM custody_successor_rebinds
              WHERE rebind_id = ?1 AND side = 'sender'",
                [&receipt.rebind_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let rebind: CustodySuccessorRebind =
            serde_json::from_slice(&rebind_json).map_err(|_| {
                WorkLedgerError::Refused("stored custody successor is invalid".to_owned())
            })?;
        if authenticated.authenticated_peer_machine_ref != rebind.target_machine_ref
            || successor_receipt(&rebind)? != *receipt
        {
            return Err(WorkLedgerError::Refused(
                "custody successor receipt does not match the prepared migration".to_owned(),
            ));
        }
        if state == "acknowledged" {
            return if stored_receipt.as_deref() == Some(&receipt.receipt_digest) {
                Ok(())
            } else {
                Err(WorkLedgerError::Refused(
                    "acknowledged custody successor receipt is contradictory".to_owned(),
                ))
            };
        }
        if state != "prepared" {
            return Err(WorkLedgerError::Refused(
                "custody successor receipt arrived outside its prepared transition".to_owned(),
            ));
        }
        if require_live_lease {
            super::super::ownership_lease::validate_live_custody_lease_tuple(
                &tx,
                &rebind,
                Utc::now(),
            )?;
        }
        let (outbox_state, active_epoch): (String, i64) = tx.query_row(
            "SELECT state, active_rebind_epoch FROM custody_outbox WHERE message_id = ?1",
            [&rebind.message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if outbox_state != "custody_accepted"
            || positive_u64("custody successor active epoch", active_epoch)?
                != rebind.old_authority_epoch
        {
            return Err(WorkLedgerError::Refused(
                "custody successor lost its source authority race".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        tx.execute(
            "INSERT INTO custody_rebinds
             (message_id, epoch, target_machine_ref, target_incarnation_ref, target_route_ref,
              terminal_adapter, authority_digest, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                rebind.message_id,
                rebind.new_authority_epoch,
                rebind.target_machine_ref,
                rebind.new_target_incarnation_ref,
                rebind.new_target_route_ref,
                rebind.terminal_adapter,
                rebind.new_authority_digest,
                now
            ],
        )?;
        tx.execute(
            "UPDATE custody_outbox SET active_rebind_epoch = ?2,
                    custody_receipt_digest = ?3, custody_transfer_digest = ?4, updated_at = ?5
              WHERE message_id = ?1 AND state = 'custody_accepted'",
            params![
                rebind.message_id,
                rebind.new_authority_epoch,
                receipt.receipt_digest,
                rebind.rebind_digest,
                now
            ],
        )?;
        let receipt_json = serde_json::to_vec(receipt).map_err(|_| {
            WorkLedgerError::Refused("custody successor receipt cannot be serialized".to_owned())
        })?;
        tx.execute(
            "UPDATE custody_successor_rebinds SET state = 'acknowledged', receipt_json = ?2,
                    receipt_digest = ?3, updated_at = ?4
              WHERE rebind_id = ?1 AND side = 'sender' AND state = 'prepared'",
            params![receipt.rebind_id, receipt_json, receipt.receipt_digest, now],
        )?;
        record_custody_event(
            &tx,
            &rebind.message_id,
            "sender",
            "successor_acknowledged",
            &receipt.receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn mark_custody_successor_finalized(
        &self,
        rebind_id: &str,
        receipt_digest: &str,
    ) -> WorkLedgerResult<()> {
        validate_opaque_ref("custody successor rebind", rebind_id, "cr")?;
        validate_digest("custody successor final receipt", receipt_digest)?;
        let (_writer_domain, mut connection) = self.custody_write_connection()?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, stored): (String, Option<String>) = tx.query_row(
            "SELECT state, receipt_digest FROM custody_successor_rebinds
              WHERE rebind_id = ?1 AND side = 'sender'",
            [rebind_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if state == "finalized" && stored.as_deref() == Some(receipt_digest) {
            return Ok(());
        }
        if state != "acknowledged" || stored.as_deref() != Some(receipt_digest) {
            return Err(WorkLedgerError::Refused(
                "custody successor final acknowledgement is contradictory".to_owned(),
            ));
        }
        let now = Utc::now().to_rfc3339();
        if tx.execute(
            "UPDATE custody_successor_rebinds SET state = 'finalized', updated_at = ?2
              WHERE rebind_id = ?1 AND side = 'sender' AND state = 'acknowledged'",
            params![rebind_id, now],
        )? != 1
        {
            return Err(WorkLedgerError::Refused(
                "custody successor changed during final acknowledgement".to_owned(),
            ));
        }
        let message_id: String = tx.query_row(
            "SELECT message_id FROM custody_successor_rebinds
              WHERE rebind_id = ?1 AND side = 'sender'",
            [rebind_id],
            |row| row.get(0),
        )?;
        record_custody_event(
            &tx,
            &message_id,
            "sender",
            "successor_finalized",
            receipt_digest,
            &now,
        )?;
        tx.commit()?;
        Ok(())
    }
}
