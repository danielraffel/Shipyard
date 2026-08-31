//! Exact-route, zero-write custody inventory bindings.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::inventory::{
    immutable_ledger_query, inventory_from_connection, validate_remote_inventory,
};
use super::{
    CustodyEnvelope, LocalWorkInventory, WorkLedgerError, WorkLedgerResult, digest,
    validate_digest, validate_opaque_ref,
};

/// Full source/target/rebind/transfer identity selected only from durable custody.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyInventoryBinding {
    pub(crate) message_id: String,
    pub(crate) identity_digest: String,
    pub(crate) source_machine_ref: String,
    pub(crate) source_incarnation_ref: String,
    pub(crate) target_machine_ref: String,
    pub(crate) target_incarnation_ref: String,
    pub(crate) target_route_ref: String,
    pub(crate) terminal_adapter: String,
    pub(crate) rebind_epoch: u64,
    pub(crate) authority_digest: String,
    pub(crate) transfer_digest: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CustodyInventoryResolution {
    Query(Box<CustodyInventoryBinding>),
    Uncertain(&'static str),
    Refused(&'static str),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CustodyInventoryWireRequest {
    pub(crate) binding: CustodyInventoryBinding,
    pub(crate) request_digest: String,
}

#[derive(Serialize)]
struct RequestIdentity<'a> {
    domain: &'static str,
    binding: &'a CustodyInventoryBinding,
}

pub(crate) fn custody_inventory_request(
    state_dir: &std::path::Path,
    message_id: &str,
) -> WorkLedgerResult<CustodyInventoryResolution> {
    validate_opaque_ref("custody inventory message", message_id, "wm")?;
    let Some(resolution) = immutable_ledger_query(state_dir, |connection, _snapshot| {
        resolve_from_connection(connection, message_id)
    })?
    else {
        return Ok(CustodyInventoryResolution::Refused(
            "custody-message-missing",
        ));
    };
    Ok(resolution)
}

type StoredCustodyRoute = (
    Vec<u8>,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    String,
    String,
);

#[allow(clippy::too_many_lines)] // One exact join and validation pass keeps state and route inseparable.
fn resolve_from_connection(
    connection: &Connection,
    message_id: &str,
) -> WorkLedgerResult<CustodyInventoryResolution> {
    let row: Option<StoredCustodyRoute> = connection
        .query_row(
            "SELECT message.identity_json, message.identity_digest, message.state,
                    message.active_rebind_epoch, message.custody_transfer_digest,
                    message.custody_receipt_digest, message.processed_receipt_digest,
                    route.target_machine_ref, route.target_incarnation_ref,
                    route.target_route_ref, route.terminal_adapter, route.authority_digest
               FROM custody_outbox message
               JOIN custody_rebinds route
                 ON route.message_id = message.message_id
                AND route.epoch = message.active_rebind_epoch
              WHERE message.message_id = ?1",
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
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                ))
            },
        )
        .optional()?;
    let Some((
        identity_json,
        identity_digest,
        state,
        epoch,
        transfer_digest,
        custody_receipt_digest,
        processed_receipt_digest,
        target_machine_ref,
        target_incarnation_ref,
        target_route_ref,
        terminal_adapter,
        authority_digest,
    )) = row
    else {
        return Ok(CustodyInventoryResolution::Refused(
            "custody-message-missing-or-contradictory",
        ));
    };
    let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json)
        .map_err(|_| WorkLedgerError::Refused("stored custody identity is malformed".to_owned()))?;
    envelope.validate()?;
    let rebind_epoch = u64::try_from(epoch)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| WorkLedgerError::Refused("custody rebind epoch is invalid".to_owned()))?;
    validate_opaque_ref(
        "custody inventory target machine",
        &target_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "custody inventory target incarnation",
        &target_incarnation_ref,
        "incarnation",
    )?;
    validate_opaque_ref("custody inventory target route", &target_route_ref, "route")?;
    validate_digest("custody inventory authority", &authority_digest)?;
    if envelope.message_id != message_id || envelope.identity_digest != identity_digest {
        return Ok(CustodyInventoryResolution::Refused(
            "custody-identity-contradictory",
        ));
    }
    for digest in [
        transfer_digest.as_deref(),
        custody_receipt_digest.as_deref(),
        processed_receipt_digest.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        validate_digest("custody lifecycle evidence", digest)?;
    }
    match state.as_str() {
        "pending" | "claimed" => {
            if transfer_digest.is_some()
                || custody_receipt_digest.is_some()
                || processed_receipt_digest.is_some()
            {
                return Ok(CustodyInventoryResolution::Refused(
                    "custody-state-contradictory",
                ));
            }
            return Ok(CustodyInventoryResolution::Uncertain(
                "custody-not-yet-accepted",
            ));
        }
        "cancelled" | "superseded" => {
            return Ok(CustodyInventoryResolution::Refused(
                "custody-message-terminally-invalid",
            ));
        }
        "custody_accepted"
            if transfer_digest.is_some()
                && custody_receipt_digest.is_some()
                && processed_receipt_digest.is_none() => {}
        "processed"
            if transfer_digest.is_some()
                && custody_receipt_digest.is_some()
                && processed_receipt_digest.is_some() => {}
        _ => {
            return Ok(CustodyInventoryResolution::Refused(
                "custody-state-contradictory",
            ));
        }
    }
    let transfer_digest = transfer_digest.ok_or_else(|| {
        WorkLedgerError::Refused("accepted custody has no transfer digest".to_owned())
    })?;
    let binding = CustodyInventoryBinding {
        message_id: message_id.to_owned(),
        identity_digest,
        source_machine_ref: envelope.source_machine_ref,
        source_incarnation_ref: envelope.source_incarnation_ref,
        target_machine_ref,
        target_incarnation_ref,
        target_route_ref,
        terminal_adapter,
        rebind_epoch,
        authority_digest,
        transfer_digest,
    };
    validate_binding(&binding)?;
    Ok(CustodyInventoryResolution::Query(Box::new(binding)))
}

impl CustodyInventoryWireRequest {
    pub(crate) fn new(binding: CustodyInventoryBinding) -> WorkLedgerResult<Self> {
        validate_binding(&binding)?;
        let request_digest = request_digest(&binding)?;
        Ok(Self {
            binding,
            request_digest,
        })
    }

    pub(crate) fn validate(&self) -> WorkLedgerResult<()> {
        validate_binding(&self.binding)?;
        validate_digest("custody inventory request", &self.request_digest)?;
        if request_digest(&self.binding)? != self.request_digest {
            return Err(WorkLedgerError::Refused(
                "custody inventory request binding mismatch".to_owned(),
            ));
        }
        Ok(())
    }
}

fn request_digest(binding: &CustodyInventoryBinding) -> WorkLedgerResult<String> {
    serde_json::to_vec(&RequestIdentity {
        domain: "shipyard.custody-inventory.request.v1",
        binding,
    })
    .map(|encoded| digest(&encoded))
    .map_err(|_| WorkLedgerError::Refused("custody inventory request cannot be encoded".to_owned()))
}

fn validate_binding(binding: &CustodyInventoryBinding) -> WorkLedgerResult<()> {
    validate_opaque_ref("custody inventory message", &binding.message_id, "wm")?;
    validate_digest("custody inventory identity", &binding.identity_digest)?;
    validate_opaque_ref(
        "custody inventory source machine",
        &binding.source_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "custody inventory source incarnation",
        &binding.source_incarnation_ref,
        "incarnation",
    )?;
    validate_opaque_ref(
        "custody inventory target machine",
        &binding.target_machine_ref,
        "machine",
    )?;
    validate_opaque_ref(
        "custody inventory target incarnation",
        &binding.target_incarnation_ref,
        "incarnation",
    )?;
    validate_opaque_ref(
        "custody inventory target route",
        &binding.target_route_ref,
        "route",
    )?;
    validate_digest("custody inventory authority", &binding.authority_digest)?;
    validate_digest("custody inventory transfer", &binding.transfer_digest)?;
    if binding.rebind_epoch == 0
        || binding.terminal_adapter.is_empty()
        || binding.terminal_adapter.len() > 512
        || !binding.terminal_adapter.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
        })
    {
        return Err(WorkLedgerError::Refused(
            "custody inventory endpoint is invalid".to_owned(),
        ));
    }
    Ok(())
}

pub(crate) fn verify_custody_inventory_inbox(
    state_dir: &std::path::Path,
    authenticated_peer_machine_ref: &str,
    request: &CustodyInventoryWireRequest,
) -> WorkLedgerResult<LocalWorkInventory> {
    request.validate()?;
    if authenticated_peer_machine_ref != request.binding.source_machine_ref {
        return Err(WorkLedgerError::Refused(
            "custody inventory source peer mismatch".to_owned(),
        ));
    }
    let inventory = immutable_ledger_query(state_dir, |connection, snapshot| {
        let row: Option<(Vec<u8>, String)> = connection
            .query_row(
                "SELECT identity_json, state FROM custody_inbox
              WHERE message_id = ?1 AND identity_digest = ?2
                AND rebind_epoch = ?3 AND target_machine_ref = ?4
                AND target_incarnation_ref = ?5 AND target_route_ref = ?6
                AND terminal_adapter = ?7 AND authority_digest = ?8
                AND transfer_digest = ?9",
                params![
                    request.binding.message_id,
                    request.binding.identity_digest,
                    request.binding.rebind_epoch,
                    request.binding.target_machine_ref,
                    request.binding.target_incarnation_ref,
                    request.binding.target_route_ref,
                    request.binding.terminal_adapter,
                    request.binding.authority_digest,
                    request.binding.transfer_digest,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((identity_json, state)) = row else {
            return Ok(None);
        };
        if !matches!(state.as_str(), "received" | "processing" | "processed") {
            return Ok(None);
        }
        let envelope: CustodyEnvelope = serde_json::from_slice(&identity_json).map_err(|_| {
            WorkLedgerError::Refused("custody inbox identity is malformed".to_owned())
        })?;
        envelope.validate()?;
        let matched = envelope.message_id == request.binding.message_id
            && envelope.identity_digest == request.binding.identity_digest
            && envelope.source_machine_ref == request.binding.source_machine_ref
            && envelope.source_incarnation_ref == request.binding.source_incarnation_ref;
        matched
            .then(|| inventory_from_connection(connection, snapshot.to_owned()))
            .transpose()
    })?
    .flatten();
    let Some(inventory) = inventory else {
        return Err(WorkLedgerError::Refused(
            "custody inventory inbox binding mismatch".to_owned(),
        ));
    };
    Ok(inventory)
}

pub(crate) fn verify_custody_inventory_response(
    expected: &CustodyInventoryWireRequest,
    authenticated_peer_machine_ref: &str,
    response_request_digest: &str,
    responding_machine_ref: &str,
    inventory: &LocalWorkInventory,
) -> WorkLedgerResult<()> {
    expected.validate()?;
    validate_digest(
        "custody inventory response request",
        response_request_digest,
    )?;
    if response_request_digest != expected.request_digest
        || authenticated_peer_machine_ref != expected.binding.target_machine_ref
        || responding_machine_ref != expected.binding.target_machine_ref
    {
        return Err(WorkLedgerError::Refused(
            "custody inventory response binding mismatch".to_owned(),
        ));
    }
    validate_remote_inventory(inventory)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding() -> CustodyInventoryBinding {
        CustodyInventoryBinding {
            message_id: format!("wm_{}", "a".repeat(64)),
            identity_digest: "b".repeat(64),
            source_machine_ref: format!("machine_{}", "c".repeat(64)),
            source_incarnation_ref: format!("incarnation_{}", "d".repeat(64)),
            target_machine_ref: format!("machine_{}", "e".repeat(64)),
            target_incarnation_ref: format!("incarnation_{}", "f".repeat(64)),
            target_route_ref: format!("route_{}", "1".repeat(64)),
            terminal_adapter: "cmux".to_owned(),
            rebind_epoch: 7,
            authority_digest: "2".repeat(64),
            transfer_digest: "3".repeat(64),
        }
    }

    #[test]
    fn request_digest_binds_every_source_target_rebind_and_transfer_field() {
        let request = CustodyInventoryWireRequest::new(binding()).unwrap();
        let mut mutations = Vec::new();
        for index in 0..11 {
            let mut mutated = request.clone();
            match index {
                0 => mutated.binding.message_id = format!("wm_{}", "4".repeat(64)),
                1 => mutated.binding.identity_digest = "4".repeat(64),
                2 => mutated.binding.source_machine_ref = format!("machine_{}", "4".repeat(64)),
                3 => {
                    mutated.binding.source_incarnation_ref =
                        format!("incarnation_{}", "4".repeat(64));
                }
                4 => mutated.binding.target_machine_ref = format!("machine_{}", "4".repeat(64)),
                5 => {
                    mutated.binding.target_incarnation_ref =
                        format!("incarnation_{}", "4".repeat(64));
                }
                6 => mutated.binding.target_route_ref = format!("route_{}", "4".repeat(64)),
                7 => mutated.binding.terminal_adapter = "other".to_owned(),
                8 => mutated.binding.rebind_epoch += 1,
                9 => mutated.binding.authority_digest = "4".repeat(64),
                10 => mutated.binding.transfer_digest = "4".repeat(64),
                _ => unreachable!(),
            }
            mutations.push(mutated);
        }
        assert!(
            mutations
                .iter()
                .all(|mutation| mutation.validate().is_err())
        );
    }
}
