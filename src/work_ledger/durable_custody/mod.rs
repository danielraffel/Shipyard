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
    release_inbox_claim, release_sender_claim, sqlite_i64, transfer_digest, transfer_from_tx,
    validate_control, validate_control_receipt, validate_lease,
    validate_persisted_processed_receipts, validate_processed_receipt, validate_receipt,
    validate_target, validate_transfer, verify_inbox_claim, verify_sender_claim,
};

pub(super) fn validate_persisted_custody(connection: &Connection) -> WorkLedgerResult<()> {
    validate_persisted_processed_receipts(connection)
}
mod receiver;
mod sender;
