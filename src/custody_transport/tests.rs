use std::collections::BTreeMap;

use super::*;

#[test]
fn absent_or_isolated_policy_is_disabled_without_state() {
    let temp = tempfile::tempdir().unwrap();
    assert!(
        load_custody_transport_policy(RuntimeMode::Isolated, temp.path().into())
            .unwrap()
            .is_none()
    );
    assert!(
        load_custody_transport_policy(RuntimeMode::Shipyard, temp.path().into())
            .unwrap()
            .is_none()
    );
}

#[test]
fn malformed_incoming_request_is_redacted_and_has_zero_ledger_effect() {
    let temp = tempfile::tempdir().unwrap();
    let policy = test_policy(temp.path(), true);
    let response = handle_incoming_request(
        &policy,
        temp.path(),
        &IncomingPeerEvidence {
            peer_machine_ref: policy.peers.keys().next().unwrap().clone(),
            ssh_connection_present: true,
            ssh_auth_key_sha256: policy
                .peers
                .values()
                .next()
                .unwrap()
                .ssh_auth_key_sha256
                .clone(),
        },
        b"token=secret",
    );
    let encoded = serde_json::to_string(&response).unwrap();
    assert!(encoded.contains("custody-request-malformed"));
    assert!(!encoded.contains("secret"));
    assert!(!temp.path().join("work-ledger.sqlite3").exists());
}

#[test]
fn only_elected_machine_accepts_mutating_delivery() {
    let temp = tempfile::tempdir().unwrap();
    let policy = test_policy(temp.path(), false);
    assert_eq!(
        require_local_mutation_authority(&policy),
        Err("custody-local-machine-is-not-mutation-authority".to_owned())
    );
}

fn test_policy(root: &Path, local_is_authority: bool) -> CustodyTransportPolicy {
    let local = opaque("machine", "local");
    let peer = opaque("machine", "peer");
    let peer_policy = CustodyPeer {
        machine_ref: peer.clone(),
        ssh_program: "/usr/bin/ssh".into(),
        destination: "peer".to_owned(),
        known_hosts_file: root.join("known-hosts"),
        identity_file: root.join("identity"),
        port: 22,
        remote_subsystem: "shipyard-custody-v1".to_owned(),
        ssh_auth_key_sha256: "b".repeat(64),
        successor_proof_digest: "c".repeat(64),
    };
    CustodyTransportPolicy {
        local_machine_ref: local.clone(),
        local_incarnation_ref: opaque("incarnation", "local"),
        mutation_authority_machine_ref: if local_is_authority {
            local
        } else {
            peer.clone()
        },
        authority_digest: "d".repeat(64),
        sender_owner_ref: opaque("owner", "sender"),
        lease_seconds: 30,
        deadline_seconds: 5,
        max_output_bytes: 4096,
        peers: BTreeMap::from([(peer, peer_policy)]),
        policy_digest: "e".repeat(64),
    }
}

fn opaque(prefix: &str, seed: &str) -> String {
    format!("{prefix}_{}", hex::encode(Sha256::digest(seed.as_bytes())))
}
