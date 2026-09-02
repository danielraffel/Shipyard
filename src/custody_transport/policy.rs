use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::readiness::{
    MAX_BYTES as MAX_READINESS_BYTES, Receipt as ReadinessReceipt,
    SCHEMA_VERSION as READINESS_SCHEMA_VERSION,
};
use super::{
    MAX_CUSTODY_WIRE_BYTES, absolute_path, required, safe_token, validate_digest, validate_opaque,
};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;

const POLICY_KEY: &str = "custody_transport";
const MAX_PEERS: usize = 32;
pub(super) const SETUP_CONTRACT_VERSION: u32 = 1;
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPolicy {
    #[serde(default)]
    pub(super) enabled: bool,
    /// Explicit marker for the guarded setup contract. Its absence denotes
    /// the pre-setup legacy policy shape and is migration-required in doctor.
    #[serde(default)]
    pub(super) setup_contract_version: Option<u32>,
    pub(super) local_machine_ref: Option<String>,
    pub(super) local_incarnation_ref: Option<String>,
    pub(super) local_route_ref: Option<String>,
    pub(super) local_terminal_adapter: Option<String>,
    pub(super) mutation_authority_machine_ref: Option<String>,
    pub(super) authority_digest: Option<String>,
    pub(super) sender_owner_ref: Option<String>,
    pub(super) inbox_owner_ref: Option<String>,
    pub(super) lease_seconds: Option<u64>,
    pub(super) deadline_seconds: Option<u64>,
    pub(super) max_output_bytes: Option<u64>,
    /// Optional protected receiver configuration checked by `custody doctor`.
    /// These values are intentionally not consumed by the transport runtime;
    /// keeping them in the machine-global policy lets the setup/doctor path
    /// validate the receiver without granting Shipyard authority to mutate
    /// `sshd` or its key files.
    #[serde(default)]
    pub(super) sshd_config_file: Option<String>,
    #[serde(default)]
    pub(super) authorized_keys_file: Option<String>,
    /// Owner-attested readiness digests. Shipyard records and checks these
    /// fences but never invents a bootstrap, publication, or profile receipt.
    #[serde(default)]
    pub(super) destination_bootstrap_digest: Option<String>,
    #[serde(default)]
    pub(super) native_publication_digest: Option<String>,
    #[serde(default)]
    pub(super) profile_digest: Option<String>,
    pub(super) receiver_program: Option<String>,
    pub(super) destination_bootstrap_receipt_file: Option<String>,
    pub(super) native_publication_receipt_file: Option<String>,
    pub(super) profile_receipt_file: Option<String>,
    pub(super) peers: Option<Vec<RawPeer>>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawPeer {
    pub(super) machine_ref: String,
    pub(super) incarnation_ref: String,
    pub(super) route_ref: String,
    pub(super) terminal_adapter: String,
    pub(super) ssh_program: String,
    pub(super) destination: String,
    pub(super) known_hosts_file: String,
    pub(super) identity_file: String,
    /// Digest of this host's outbound private identity public key.
    #[serde(default)]
    pub(super) outbound_identity_sha256: Option<String>,
    /// Owner-provided public key used by the peer when authenticating inbound.
    #[serde(default)]
    pub(super) inbound_public_key_file: Option<String>,
    pub(super) port: u16,
    pub(super) remote_subsystem: String,
    pub(super) ssh_auth_key_sha256: String,
    #[serde(default)]
    pub(super) successor_proof_digest: Option<String>,
}

impl RawPolicy {
    fn has_setup_extensions(&self) -> bool {
        self.sshd_config_file.is_some()
            || self.authorized_keys_file.is_some()
            || self.destination_bootstrap_digest.is_some()
            || self.native_publication_digest.is_some()
            || self.profile_digest.is_some()
            || self.receiver_program.is_some()
            || self.destination_bootstrap_receipt_file.is_some()
            || self.native_publication_receipt_file.is_some()
            || self.profile_receipt_file.is_some()
            || self.peers.as_ref().is_some_and(|peers| {
                peers.iter().any(|peer| {
                    peer.outbound_identity_sha256.is_some()
                        || peer.inbound_public_key_file.is_some()
                })
            })
    }
}

#[derive(Clone, Debug)]
#[cfg_attr(
    all(windows, not(test)),
    expect(
        dead_code,
        reason = "outbound custody fields are consumed only by the Unix transport runtime"
    )
)]
pub(crate) struct CustodyTransportPolicy {
    pub(super) local_machine_ref: String,
    pub(super) local_incarnation_ref: String,
    pub(super) local_route_ref: String,
    pub(super) local_terminal_adapter: String,
    pub(super) mutation_authority_machine_ref: String,
    pub(super) authority_digest: String,
    pub(super) sender_owner_ref: String,
    pub(super) inbox_owner_ref: String,
    pub(super) lease_seconds: u64,
    pub(super) deadline_seconds: u64,
    pub(super) max_output_bytes: u64,
    pub(super) peers: BTreeMap<String, CustodyPeer>,
    pub(super) policy_digest: String,
}

#[derive(Clone, Debug)]
#[cfg_attr(
    windows,
    expect(
        dead_code,
        reason = "outbound peer fields are consumed only by the Unix transport runtime"
    )
)]
pub(super) struct CustodyPeer {
    pub(super) machine_ref: String,
    pub(super) incarnation_ref: String,
    pub(super) route_ref: String,
    pub(super) terminal_adapter: String,
    pub(super) ssh_program: PathBuf,
    pub(super) destination: String,
    pub(super) known_hosts_file: PathBuf,
    pub(super) identity_file: PathBuf,
    pub(super) outbound_identity_sha256: String,
    pub(super) inbound_public_key_file: PathBuf,
    pub(super) port: u16,
    pub(super) remote_subsystem: String,
    pub(super) ssh_auth_key_sha256: String,
}

struct LocalCustodyPolicy {
    machine_ref: String,
    incarnation_ref: String,
    route_ref: String,
    terminal_adapter: String,
    mutation_authority_machine_ref: String,
    authority_digest: String,
    sender_owner_ref: String,
    inbox_owner_ref: String,
}

#[allow(clippy::too_many_lines)]
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
    let marked_setup = match raw.setup_contract_version {
        None => false,
        Some(version) if version == SETUP_CONTRACT_VERSION => true,
        Some(_) => return Err("custody-policy-setup-contract-unknown".to_owned()),
    };
    if !raw.enabled {
        return Ok(None);
    }
    if !marked_setup && raw.has_setup_extensions() {
        return Err("custody-policy-migration-required".to_owned());
    }
    let local = validate_local_policy(&raw)?;
    if marked_setup {
        for path in [
            required(raw.sshd_config_file.clone(), "sshd-config")?,
            required(raw.authorized_keys_file.clone(), "authorized-keys")?,
            required(raw.receiver_program.clone(), "receiver-program")?,
        ] {
            absolute_path(&path)?;
        }
        for digest in [
            required(
                raw.destination_bootstrap_digest.clone(),
                "destination-bootstrap-digest",
            )?,
            required(
                raw.native_publication_digest.clone(),
                "native-publication-digest",
            )?,
            required(raw.profile_digest.clone(), "profile-digest")?,
        ] {
            validate_digest(&digest)?;
        }
        for receipt in [
            required(
                raw.destination_bootstrap_receipt_file.clone(),
                "destination-bootstrap-receipt-file",
            )?,
            required(
                raw.native_publication_receipt_file.clone(),
                "native-publication-receipt-file",
            )?,
            required(raw.profile_receipt_file.clone(), "profile-receipt-file")?,
        ] {
            let path = absolute_path(&receipt)?;
            if !std::fs::metadata(path).is_ok_and(|meta| meta.is_file()) {
                return Err("custody-policy-readiness-receipt-unavailable".to_owned());
            }
        }
        let _readiness_receipts = [
            verify_readiness_receipt(
                raw.destination_bootstrap_receipt_file.as_deref().unwrap(),
                "destination_bootstrap",
                &local,
                &raw,
            )?,
            verify_readiness_receipt(
                raw.native_publication_receipt_file.as_deref().unwrap(),
                "native_publication",
                &local,
                &raw,
            )?,
            verify_readiness_receipt(
                raw.profile_receipt_file.as_deref().unwrap(),
                "profile",
                &local,
                &raw,
            )?,
        ];
    }
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
        validate_opaque(&peer.incarnation_ref, "incarnation")?;
        validate_opaque(&peer.route_ref, "route")?;
        validate_digest(&peer.ssh_auth_key_sha256)?;
        let outbound_identity_sha256 = match (marked_setup, peer.outbound_identity_sha256) {
            (true, Some(digest)) => {
                validate_digest(&digest)?;
                digest
            }
            (true, None) => return Err("custody-policy-outbound-key-digest-missing".to_owned()),
            (false, Some(_)) => return Err("custody-policy-migration-required".to_owned()),
            (false, None) => String::new(),
        };
        if let Some(legacy_successor_proof_digest) = &peer.successor_proof_digest {
            validate_digest(legacy_successor_proof_digest)?;
        }
        let ssh_program = absolute_path(&peer.ssh_program)?;
        let known_hosts_file = absolute_path(&peer.known_hosts_file)?;
        let identity_file = absolute_path(&peer.identity_file)?;
        let inbound_public_key_file = match (marked_setup, peer.inbound_public_key_file) {
            (true, Some(path)) => absolute_path(&path)?,
            (true, None) => return Err("custody-policy-inbound-key-file-missing".to_owned()),
            (false, Some(_)) => return Err("custody-policy-migration-required".to_owned()),
            (false, None) => PathBuf::new(),
        };
        if peer.port == 0
            || !safe_token(&peer.destination)
            || !safe_token(&peer.remote_subsystem)
            || !safe_token(&peer.terminal_adapter)
        {
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
                    incarnation_ref: peer.incarnation_ref,
                    route_ref: peer.route_ref,
                    terminal_adapter: peer.terminal_adapter,
                    ssh_program,
                    destination: peer.destination,
                    known_hosts_file,
                    identity_file,
                    outbound_identity_sha256,
                    inbound_public_key_file,
                    port: peer.port,
                    remote_subsystem: peer.remote_subsystem,
                    ssh_auth_key_sha256: peer.ssh_auth_key_sha256,
                },
            )
            .is_some()
        {
            return Err("custody-policy-peer-duplicate".to_owned());
        }
    }
    if local.mutation_authority_machine_ref != local.machine_ref
        && !canonical.contains_key(&local.mutation_authority_machine_ref)
    {
        return Err("custody-policy-mutation-authority-unknown".to_owned());
    }
    Ok(Some(CustodyTransportPolicy {
        local_machine_ref: local.machine_ref,
        local_incarnation_ref: local.incarnation_ref,
        local_route_ref: local.route_ref,
        local_terminal_adapter: local.terminal_adapter,
        mutation_authority_machine_ref: local.mutation_authority_machine_ref,
        authority_digest: local.authority_digest,
        sender_owner_ref: local.sender_owner_ref,
        inbox_owner_ref: local.inbox_owner_ref,
        lease_seconds,
        deadline_seconds,
        max_output_bytes,
        peers: canonical,
        policy_digest,
    }))
}

fn validate_local_policy(raw: &RawPolicy) -> Result<LocalCustodyPolicy, String> {
    let local = LocalCustodyPolicy {
        machine_ref: required(raw.local_machine_ref.clone(), "local-machine")?,
        incarnation_ref: required(raw.local_incarnation_ref.clone(), "local-incarnation")?,
        route_ref: required(raw.local_route_ref.clone(), "local-route")?,
        terminal_adapter: required(raw.local_terminal_adapter.clone(), "local-terminal-adapter")?,
        mutation_authority_machine_ref: required(
            raw.mutation_authority_machine_ref.clone(),
            "mutation-authority-machine",
        )?,
        authority_digest: required(raw.authority_digest.clone(), "authority-digest")?,
        sender_owner_ref: required(raw.sender_owner_ref.clone(), "sender-owner")?,
        inbox_owner_ref: required(raw.inbox_owner_ref.clone(), "inbox-owner")?,
    };
    validate_opaque(&local.machine_ref, "machine")?;
    validate_opaque(&local.incarnation_ref, "incarnation")?;
    validate_opaque(&local.route_ref, "route")?;
    validate_opaque(&local.mutation_authority_machine_ref, "machine")?;
    validate_opaque(&local.sender_owner_ref, "owner")?;
    validate_opaque(&local.inbox_owner_ref, "owner")?;
    validate_digest(&local.authority_digest)?;
    if !safe_token(&local.terminal_adapter) {
        return Err("custody-policy-local-terminal-adapter-invalid".to_owned());
    }
    Ok(local)
}

fn verify_readiness_receipt(
    path: &str,
    expected_kind: &str,
    local: &LocalCustodyPolicy,
    raw: &RawPolicy,
) -> Result<String, String> {
    let path = absolute_path(path)?;
    // Open and validate the same owner-only, no-follow descriptor in one
    // operation.  A metadata check followed by `fs::read` would permit a
    // symlink swap between checks and consume an untrusted receipt.
    let bytes =
        crate::custody_transport::setup::read_private_input(&path, MAX_READINESS_BYTES, true)
            .map_err(|error| match error {
                crate::custody_transport::setup::ReadPrivateError::Missing => {
                    "custody-policy-readiness-receipt-unavailable".to_owned()
                }
                crate::custody_transport::setup::ReadPrivateError::Code(code) => code.to_owned(),
            })?;
    let receipt: ReadinessReceipt = serde_json::from_slice(&bytes)
        .map_err(|_| "custody-policy-readiness-receipt-malformed".to_owned())?;
    if receipt.schema_version != READINESS_SCHEMA_VERSION
        || receipt.kind != expected_kind
        || receipt.machine_ref != local.machine_ref
        || receipt.incarnation_ref != local.incarnation_ref
        || receipt.route_ref != local.route_ref
        || receipt.authority_digest != local.authority_digest
        || receipt.destination_bootstrap_digest
            != raw.destination_bootstrap_digest.clone().unwrap_or_default()
        || receipt.profile_digest != raw.profile_digest.clone().unwrap_or_default()
        || receipt.native_publication_digest
            != raw.native_publication_digest.clone().unwrap_or_default()
        || validate_digest(&receipt.payload_digest).is_err()
    {
        return Err("custody-policy-readiness-receipt-binding-mismatch".to_owned());
    }
    let canonical = serde_json::json!({
        "schema_version": receipt.schema_version,
        "kind": receipt.kind,
        "machine_ref": receipt.machine_ref,
        "incarnation_ref": receipt.incarnation_ref,
        "route_ref": receipt.route_ref,
        "authority_digest": receipt.authority_digest,
        "destination_bootstrap_digest": receipt.destination_bootstrap_digest,
        "profile_digest": receipt.profile_digest,
        "native_publication_digest": receipt.native_publication_digest,
    });
    let computed = hex::encode(Sha256::digest(
        serde_json::to_vec(&canonical)
            .map_err(|_| "custody-policy-readiness-receipt-malformed".to_owned())?,
    ));
    if computed != receipt.payload_digest {
        return Err("custody-policy-readiness-receipt-digest-mismatch".to_owned());
    }
    Ok(hex::encode(Sha256::digest(bytes)))
}
