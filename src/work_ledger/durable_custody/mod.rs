//! Durable, transport-neutral custody transfer between Shipyard hosts.
//!
//! Each host owns a local WAL database. Only authenticated envelopes cross
//! machines; shared or network-mounted `SQLite` is never a transport.

use chrono::{DateTime, Utc};
use rusqlite::OptionalExtension;

use super::{
    Connection, Transaction, TransactionBehavior, WorkLedger, WorkLedgerError, WorkLedgerResult,
    configure_durable, digest, opaque_ref, params, validate_digest, validate_opaque_ref,
    validate_token, verify_integrity, verify_supported_schema,
};

mod wire;
pub(crate) use wire::*;
mod common;
use common::{
    control_kind_str, control_receipt, load_control, positive_u64, processed_receipt,
    receipt_from_transfer, record_custody_event, relation_prior_state, release_expired_claims,
    release_inbox_claim, release_sender_claim, sqlite_i64, successor_receipt, transfer_digest,
    transfer_from_tx, validate_control, validate_control_receipt, validate_lease,
    validate_persisted_processed_receipts, validate_processed_receipt, validate_receipt,
    validate_successor_rebind, validate_successor_receipt, validate_target, validate_transfer,
    verify_inbox_claim, verify_sender_claim,
};

#[allow(clippy::too_many_lines)] // One fail-closed pass binds successor rows, history, and JSON.
pub(super) fn validate_persisted_custody(connection: &Connection) -> WorkLedgerResult<()> {
    validate_persisted_processed_receipts(connection)?;
    let has_successor_history: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
          WHERE type = 'table' AND name = 'custody_successor_events')",
        [],
        |row| row.get(0),
    )?;
    if !has_successor_history {
        return Ok(());
    }
    let invalid_history: i64 = connection.query_row(
        "SELECT COUNT(*) FROM custody_successor_rebinds rebind
          WHERE NOT EXISTS (
            SELECT 1 FROM custody_successor_events event
             WHERE event.rebind_id = rebind.rebind_id AND event.side = rebind.side
               AND event.sequence = (
                 SELECT max(last.sequence) FROM custody_successor_events last
                  WHERE last.rebind_id = rebind.rebind_id AND last.side = rebind.side)
               AND event.to_state = rebind.state)
             OR (SELECT count(*) FROM custody_successor_events event
                  WHERE event.rebind_id = rebind.rebind_id AND event.side = rebind.side)
                != (SELECT max(event.sequence) FROM custody_successor_events event
                     WHERE event.rebind_id = rebind.rebind_id AND event.side = rebind.side)",
        [],
        |row| row.get(0),
    )?;
    if invalid_history != 0 {
        return Err(WorkLedgerError::Refused(
            "custody successor state lacks contiguous immutable history".to_owned(),
        ));
    }
    let invalid_events: i64 = connection.query_row(
        "SELECT COUNT(*) FROM custody_successor_events event
           LEFT JOIN custody_successor_rebinds rebind
             ON rebind.rebind_id = event.rebind_id AND rebind.side = event.side
          WHERE rebind.rebind_id IS NULL
             OR event.evidence_digest != CASE
                  WHEN event.side = 'receiver' OR event.to_state IN ('acknowledged', 'finalized')
                    THEN rebind.receipt_digest ELSE rebind.rebind_digest END
             OR (event.sequence = 1
                 AND (event.from_state IS NOT NULL OR event.to_state != 'prepared'))
             OR (event.sequence > 1 AND NOT EXISTS (
                  SELECT 1 FROM custody_successor_events previous
                   WHERE previous.rebind_id = event.rebind_id
                     AND previous.side = event.side
                     AND previous.sequence = event.sequence - 1
                     AND previous.to_state = event.from_state
                     AND ((event.side = 'sender' AND event.from_state = 'prepared'
                           AND event.to_state IN ('acknowledged', 'aborted'))
                       OR (event.side = 'sender' AND event.from_state = 'acknowledged'
                           AND event.to_state = 'finalized')
                       OR (event.side = 'receiver' AND event.from_state = 'prepared'
                           AND event.to_state IN ('committed', 'aborted')))))",
        [],
        |row| row.get(0),
    )?;
    if invalid_events != 0 {
        return Err(WorkLedgerError::Refused(
            "custody successor history contains an impossible transition".to_owned(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT rebind_id, message_id, side, rebind_json, rebind_digest, authority_epoch,
                state, receipt_json, receipt_digest
           FROM custody_successor_rebinds",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, Option<Vec<u8>>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, message, side, encoded, rebind_digest, epoch, state, receipt_json, receipt_digest) in
        rows
    {
        let rebind: CustodySuccessorRebind = serde_json::from_slice(&encoded).map_err(|_| {
            WorkLedgerError::Refused("stored custody successor JSON is invalid".to_owned())
        })?;
        validate_successor_rebind(&rebind)?;
        if serde_json::to_vec(&rebind).map_err(|_| {
            WorkLedgerError::Refused("stored custody successor cannot be encoded".to_owned())
        })? != encoded
            || rebind.rebind_id != id
            || rebind.message_id != message
            || rebind.rebind_digest != rebind_digest
            || i64::try_from(rebind.new_authority_epoch).ok() != Some(epoch)
        {
            return Err(WorkLedgerError::Refused(
                "custody successor row does not bind its canonical JSON identity".to_owned(),
            ));
        }
        let expected_receipt = successor_receipt(&rebind)?;
        match (&receipt_json, &receipt_digest) {
            (Some(bytes), Some(digest)) => {
                let receipt: CustodySuccessorReceipt =
                    serde_json::from_slice(bytes).map_err(|_| {
                        WorkLedgerError::Refused(
                            "stored custody successor receipt JSON is invalid".to_owned(),
                        )
                    })?;
                validate_successor_receipt(&receipt)?;
                if serde_json::to_vec(&receipt).map_err(|_| {
                    WorkLedgerError::Refused(
                        "stored custody successor receipt cannot be encoded".to_owned(),
                    )
                })? != *bytes
                    || receipt != expected_receipt
                    || receipt.receipt_digest != *digest
                {
                    return Err(WorkLedgerError::Refused(
                        "custody successor receipt does not bind its canonical rebind".to_owned(),
                    ));
                }
            }
            (None, None)
                if side == "sender" && matches!(state.as_str(), "prepared" | "aborted") => {}
            _ => {
                return Err(WorkLedgerError::Refused(
                    "custody successor receipt/state shape is impossible".to_owned(),
                ));
            }
        }
        let bound: bool = match side.as_str() {
            "sender" => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM custody_outbox
                  WHERE message_id = ?1 AND identity_digest = ?2)",
                params![message, rebind.identity_digest],
                |row| row.get(0),
            )?,
            "receiver" => connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM custody_inbox
                  WHERE message_id = ?1 AND identity_digest = ?2)",
                params![message, rebind.identity_digest],
                |row| row.get(0),
            )?,
            _ => false,
        };
        if !bound {
            return Err(WorkLedgerError::Refused(
                "custody successor is not bound to durable message identity".to_owned(),
            ));
        }
    }
    Ok(())
}
mod rebind;
mod receiver;
use rebind::active_inbox_binding;
mod native;
mod sender;
