use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    MAX_CUSTODY_WIRE_BYTES, absolute_path, required, safe_token, validate_digest, validate_opaque,
};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;

const POLICY_KEY: &str = "custody_transport";
const MAX_PEERS: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawPolicy {
    #[serde(default)]
    enabled: bool,
    local_machine_ref: Option<String>,
    local_incarnation_ref: Option<String>,
    mutation_authority_machine_ref: Option<String>,
    authority_digest: Option<String>,
    sender_owner_ref: Option<String>,
    lease_seconds: Option<u64>,
    deadline_seconds: Option<u64>,
    max_output_bytes: Option<u64>,
    peers: Option<Vec<RawPeer>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawPeer {
    machine_ref: String,
    ssh_program: String,
    destination: String,
    known_hosts_file: String,
    identity_file: String,
    port: u16,
    remote_subsystem: String,
    ssh_auth_key_sha256: String,
    successor_proof_digest: String,
}

#[derive(Clone, Debug)]
pub(crate) struct CustodyTransportPolicy {
    pub(super) local_machine_ref: String,
    pub(super) local_incarnation_ref: String,
    pub(super) mutation_authority_machine_ref: String,
    pub(super) authority_digest: String,
    pub(super) sender_owner_ref: String,
    pub(super) lease_seconds: u64,
    pub(super) deadline_seconds: u64,
    pub(super) max_output_bytes: u64,
    pub(super) peers: BTreeMap<String, CustodyPeer>,
    pub(super) policy_digest: String,
}

#[derive(Clone, Debug)]
pub(super) struct CustodyPeer {
    pub(super) machine_ref: String,
    pub(super) ssh_program: PathBuf,
    pub(super) destination: String,
    pub(super) known_hosts_file: PathBuf,
    pub(super) identity_file: PathBuf,
    pub(super) port: u16,
    pub(super) remote_subsystem: String,
    pub(super) ssh_auth_key_sha256: String,
    pub(super) successor_proof_digest: String,
}

pub(crate) fn load_custody_transport_policy(
    mode: RuntimeMode,
    global_dir: PathBuf,
) -> Result<Option<CustodyTransportPolicy>, String> {
    if mode != RuntimeMode::Shipyard {
        return Ok(None);
    }
    let loaded = LoadedConfig::load_machine_global_from_dir(global_dir)
        .map_err(|_| "custody-policy-unavailable".to_owned())?;
    let Some(value) = loaded.get(POLICY_KEY) else {
        return Ok(None);
    };
    let raw: RawPolicy = value
        .clone()
        .try_into()
        .map_err(|_| "custody-policy-malformed".to_owned())?;
    let raw_digest = serde_json::to_vec(&raw).map_err(|_| "custody-policy-malformed".to_owned())?;
    if !raw.enabled {
        return Ok(None);
    }
    let local_machine_ref = required(raw.local_machine_ref, "local-machine")?;
    let local_incarnation_ref = required(raw.local_incarnation_ref, "local-incarnation")?;
    let mutation_authority_machine_ref = required(
        raw.mutation_authority_machine_ref,
        "mutation-authority-machine",
    )?;
    let authority_digest = required(raw.authority_digest, "authority-digest")?;
    let sender_owner_ref = required(raw.sender_owner_ref, "sender-owner")?;
    validate_opaque(&local_machine_ref, "machine")?;
    validate_opaque(&local_incarnation_ref, "incarnation")?;
    validate_opaque(&mutation_authority_machine_ref, "machine")?;
    validate_opaque(&sender_owner_ref, "owner")?;
    validate_digest(&authority_digest)?;
    let lease_seconds = raw.lease_seconds.unwrap_or(30);
    let deadline_seconds = raw.deadline_seconds.unwrap_or(12);
    let max_output_bytes = raw.max_output_bytes.unwrap_or(256 * 1024);
    if !(5..=300).contains(&lease_seconds)
        || !(1..=30).contains(&deadline_seconds)
        || !(1024..=MAX_CUSTODY_WIRE_BYTES).contains(&max_output_bytes)
        || deadline_seconds >= lease_seconds
    {
        return Err("custody-policy-bounds-invalid".to_owned());
    }
    let peers = raw.peers.unwrap_or_default();
    if peers.is_empty() || peers.len() > MAX_PEERS {
        return Err("custody-policy-peer-count-invalid".to_owned());
    }
    let policy_digest = hex::encode(Sha256::digest(raw_digest));
    let mut canonical = BTreeMap::new();
    let mut peer_keys = BTreeSet::new();
    for peer in peers {
        validate_opaque(&peer.machine_ref, "machine")?;
        validate_digest(&peer.ssh_auth_key_sha256)?;
        validate_digest(&peer.successor_proof_digest)?;
        let ssh_program = absolute_path(&peer.ssh_program)?;
        let known_hosts_file = absolute_path(&peer.known_hosts_file)?;
        let identity_file = absolute_path(&peer.identity_file)?;
        if peer.port == 0 || !safe_token(&peer.destination) || !safe_token(&peer.remote_subsystem) {
            return Err("custody-policy-peer-route-invalid".to_owned());
        }
        let machine = peer.machine_ref.clone();
        if !peer_keys.insert(peer.ssh_auth_key_sha256.clone()) {
            return Err("custody-policy-peer-key-duplicate".to_owned());
        }
        if canonical
            .insert(
                machine,
                CustodyPeer {
                    machine_ref: peer.machine_ref,
                    ssh_program,
                    destination: peer.destination,
                    known_hosts_file,
                    identity_file,
                    port: peer.port,
                    remote_subsystem: peer.remote_subsystem,
                    ssh_auth_key_sha256: peer.ssh_auth_key_sha256,
                    successor_proof_digest: peer.successor_proof_digest,
                },
            )
            .is_some()
        {
            return Err("custody-policy-peer-duplicate".to_owned());
        }
    }
    if mutation_authority_machine_ref != local_machine_ref
        && !canonical.contains_key(&mutation_authority_machine_ref)
    {
        return Err("custody-policy-mutation-authority-unknown".to_owned());
    }
    Ok(Some(CustodyTransportPolicy {
        local_machine_ref,
        local_incarnation_ref,
        mutation_authority_machine_ref,
        authority_digest,
        sender_owner_ref,
        lease_seconds,
        deadline_seconds,
        max_output_bytes,
        peers: canonical,
        policy_digest,
    }))
}
