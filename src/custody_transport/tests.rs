use std::collections::BTreeMap;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use super::*;
use crate::work_ledger::{
    CustodyEnvelope, CustodyRelation, NativeStewardDisposition, OwnershipAdoptionResult,
    OwnershipLeaseFence, RepoPolicy, WakeConsumerPolicy, WorkLedger, authenticate_custody_receipt,
    authenticate_custody_successor_receipt, authenticate_custody_transfer,
    native_publication_test_policy, native_publication_test_request, ownership_lease_fixture,
};

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
    assert!(
        CustodyTransportRuntime::for_daemon(
            RuntimeMode::Isolated,
            temp.path().into(),
            temp.path().into(),
        )
        .permits_local_continuation()
    );
}

#[test]
fn malformed_enabled_policy_cannot_fall_back_to_local_continuation() {
    let temp = tempfile::tempdir().unwrap();
    let runtime = CustodyTransportRuntime {
        policy: None,
        policy_refused: true,
        state_dir: temp.path().into(),
        last_error: Some("custody-policy-malformed".to_owned()),
        next_run_at: Instant::now(),
        result_rx: None,
    };
    assert!(!runtime.permits_local_continuation());
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

#[test]
fn production_path_stages_transports_consumes_and_acknowledges_exactly_once() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 43);
    seed_native_wake(authority_dir.path(), 43);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());

    let mut to_authority = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();
    let source = WorkLedger::open_existing(source_dir.path())
        .unwrap()
        .unwrap();
    let authority = WorkLedger::open_existing(authority_dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(source.custody_status().unwrap().outgoing_accepted, 1);
    assert_eq!(authority.custody_status().unwrap().incoming_received, 1);
    assert!(
        !source
            .has_authorized_pending_wake(&WakeConsumerPolicy {
                activation_enabled: true,
                dispatch_enabled: true,
                authorized_repositories: vec!["owner/repo".to_owned()],
            })
            .unwrap()
    );

    let mut to_source = LoopbackCarrier::new(
        &source_policy,
        source_dir.path(),
        authority_policy.local_machine_ref.clone(),
    );
    reconcile_once(&authority_policy, authority_dir.path(), &mut to_source).unwrap();
    assert_eq!(source.custody_status().unwrap().outgoing_processed, 1);
    assert_eq!(authority.custody_status().unwrap().incoming_processed, 1);

    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();
    reconcile_once(&authority_policy, authority_dir.path(), &mut to_source).unwrap();
    assert_eq!(source.custody_status().unwrap().outgoing_processed, 1);
    assert_eq!(authority.custody_status().unwrap().incoming_processed, 1);
}

#[test]
fn production_path_recovers_after_offline_send_and_receiver_claim_restart() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 44);
    seed_native_wake(authority_dir.path(), 44);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());

    let mut offline = RefusingCarrier;
    assert!(reconcile_once(&source_policy, source_dir.path(), &mut offline).is_err());
    expire_active_claim(source_dir.path(), "custody_sender_claims");

    let mut to_authority = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();
    let authority = WorkLedger::open_existing(authority_dir.path())
        .unwrap()
        .unwrap();
    let message_id = authority
        .native_custody_inbox_candidates(1)
        .unwrap()
        .pop()
        .unwrap();
    authority
        .claim_custody_inbox(
            &message_id,
            &authority_policy.inbox_owner_ref,
            Utc::now() + ChronoDuration::seconds(30),
        )
        .unwrap();
    drop(authority);
    expire_active_claim(authority_dir.path(), "custody_inbox_claims");

    let mut to_source = LoopbackCarrier::new(
        &source_policy,
        source_dir.path(),
        authority_policy.local_machine_ref.clone(),
    );
    reconcile_once(&authority_policy, authority_dir.path(), &mut to_source).unwrap();
    let source = WorkLedger::open_existing(source_dir.path())
        .unwrap()
        .unwrap();
    let authority = WorkLedger::open_existing(authority_dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(source.custody_status().unwrap().outgoing_processed, 1);
    assert_eq!(authority.custody_status().unwrap().incoming_processed, 1);
}

#[test]
#[allow(clippy::too_many_lines)] // One Gate0B.3 proof covers delivered and offline abort recovery.
fn gate_0b_3_transport_aborts_expired_receiver_prepare_before_retry() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 143);
    seed_native_wake(authority_dir.path(), 143);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let mut carrier = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut carrier).unwrap();
    let source = WorkLedger::open_existing(source_dir.path())
        .unwrap()
        .unwrap();
    let message_id: String = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path()))
        .unwrap()
        .query_row(
            "SELECT message_id FROM custody_outbox WHERE state = 'custody_accepted'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let successor_incarnation = opaque("incarnation", "Gate0B.3 successor one");
    let successor_proof = "c".repeat(64);
    let rebind = source
        .prepare_custody_successor_rebind_for_test(
            &message_id,
            &authority_policy.local_incarnation_ref,
            &successor_incarnation,
            &authority_policy.local_route_ref,
            &authority_policy.local_terminal_adapter,
            &authority_policy.authority_digest,
            &successor_proof,
            Utc::now() + ChronoDuration::seconds(1),
        )
        .unwrap();
    let mut successor_policy = authority_policy.clone();
    successor_policy.local_incarnation_ref = successor_incarnation;
    let prepared = deliver_request(
        &successor_policy,
        authority_dir.path(),
        &source_policy.local_machine_ref,
        &CustodyTransportRequest::SuccessorRebind {
            schema_version: SUCCESSOR_SCHEMA_VERSION,
            rebind: rebind.clone(),
        },
    );
    assert!(matches!(
        prepared,
        CustodyTransportResponse::SuccessorPrepared { .. }
    ));
    std::thread::sleep(Duration::from_millis(1_100));

    successor_policy.local_incarnation_ref =
        opaque("incarnation", "Gate0B.3 replacement after prepared expiry");
    successor_policy.local_route_ref = opaque("route", "Gate0B.3 replacement route");
    successor_policy.local_terminal_adapter = "replacement-adapter".to_owned();
    successor_policy.authority_digest = sha256(b"Gate0B.3 replacement authority");
    let mut abort_carrier = LoopbackCarrier::new(
        &successor_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    for refused_version in [1, 3] {
        abort_carrier.successor_response_schema_override =
            Some(SuccessorResponseSchemaOverride::Aborted(refused_version));
        assert!(reconcile_once(&source_policy, source_dir.path(), &mut abort_carrier).is_err());
        let source_state: String =
            rusqlite::Connection::open(WorkLedger::path_at(source_dir.path()))
                .unwrap()
                .query_row(
                    "SELECT state FROM custody_successor_rebinds
                      WHERE rebind_id = ?1 AND side = 'sender'",
                    [&rebind.rebind_id],
                    |row| row.get(0),
                )
                .unwrap();
        assert_eq!(source_state, "prepared");
    }
    abort_carrier.successor_response_schema_override = None;
    reconcile_once(&source_policy, source_dir.path(), &mut abort_carrier).unwrap();
    for root in [source_dir.path(), authority_dir.path()] {
        let connection = rusqlite::Connection::open(WorkLedger::path_at(root)).unwrap();
        let (state, last_state): (String, String) = connection
            .query_row(
                "SELECT rebind.state, event.to_state
                   FROM custody_successor_rebinds rebind
                   JOIN custody_successor_events event
                     ON event.rebind_id = rebind.rebind_id AND event.side = rebind.side
                  WHERE rebind.rebind_id = ?1
                  ORDER BY event.sequence DESC LIMIT 1",
                [&rebind.rebind_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (state.as_str(), last_state.as_str()),
            ("aborted", "aborted")
        );
    }
    let offline_incarnation = opaque("incarnation", "Gate0B.3 offline successor");
    let offline_rebind = source
        .prepare_custody_successor_rebind_for_test(
            &message_id,
            &authority_policy.local_incarnation_ref,
            &offline_incarnation,
            &authority_policy.local_route_ref,
            &authority_policy.local_terminal_adapter,
            &authority_policy.authority_digest,
            &successor_proof,
            Utc::now() + ChronoDuration::seconds(1),
        )
        .expect("first aborted epoch permits an offline successor attempt");
    std::thread::sleep(Duration::from_millis(1_100));
    let mut absent_abort_carrier = LoopbackCarrier::new(
        &successor_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut absent_abort_carrier)
        .expect("authenticated absent prepare creates receiver abort tombstone");
    let mut delayed_policy = authority_policy.clone();
    delayed_policy.local_incarnation_ref = offline_incarnation;
    assert!(matches!(
        deliver_request(
            &delayed_policy,
            authority_dir.path(),
            &source_policy.local_machine_ref,
            &CustodyTransportRequest::SuccessorRebind {
                schema_version: SUCCESSOR_SCHEMA_VERSION,
                rebind: offline_rebind,
            },
        ),
        CustodyTransportResponse::Refused { .. }
    ));
    source
        .prepare_custody_successor_rebind_for_test(
            &message_id,
            &authority_policy.local_incarnation_ref,
            &opaque("incarnation", "Gate0B.3 successor retry"),
            &authority_policy.local_route_ref,
            &authority_policy.local_terminal_adapter,
            &authority_policy.authority_digest,
            &successor_proof,
            Utc::now() + ChronoDuration::seconds(30),
        )
        .expect("absent aborted epoch no longer strands its successor");
}

#[allow(clippy::too_many_arguments)] // The helper preserves every production custody fence.
fn gate_0b_3_stage_and_accept_ordinary_custody(
    source_dir: &Path,
    source: &WorkLedger,
    receiver: &WorkLedger,
    source_policy: &CustodyTransportPolicy,
    authority_policy: &CustodyTransportPolicy,
    fence: &OwnershipLeaseFence,
    label: &str,
) -> String {
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir)).unwrap();
    let (work_generation, owner_generation, source_digest): (u64, u64, String) = connection
        .query_row(
            "SELECT ownership.work_generation, ownership.owner_generation, work.source_digest
               FROM agent_ownership ownership
               JOIN work_items work ON work.id = ownership.work_item_id
              WHERE ownership.ownership_id = ?1",
            [&fence.ownership_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    let wake_id = opaque("wake", label);
    let payload_digest = sha256(label.as_bytes());
    let now = Utc::now().to_rfc3339();
    connection
        .execute(
            "INSERT INTO outbox
             (wake_id, work_item_id, work_generation, owner_generation, state,
              route_ref, payload_digest, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, 'pending', ?5, ?6, ?7, ?7)",
            rusqlite::params![
                wake_id,
                fence.work_item_id,
                work_generation,
                owner_generation,
                opaque("route", label),
                payload_digest,
                now
            ],
        )
        .unwrap();
    let envelope = CustodyEnvelope::new(
        wake_id,
        fence.work_item_id.clone(),
        work_generation,
        owner_generation,
        payload_digest,
        source_digest,
        fence.workstream_handle.clone(),
        1,
        source_policy.local_machine_ref.clone(),
        source_policy.local_incarnation_ref.clone(),
        CustodyRelation::wake(),
    )
    .unwrap();
    let message_id = envelope.message_id.clone();
    source
        .stage_cross_machine_custody(
            &envelope,
            &authority_policy.local_machine_ref,
            &authority_policy.local_incarnation_ref,
            &authority_policy.local_route_ref,
            &authority_policy.local_terminal_adapter,
            &authority_policy.authority_digest,
        )
        .unwrap();
    let claim = source
        .claim_custody_send(
            &message_id,
            &source_policy.sender_owner_ref,
            Utc::now() + ChronoDuration::seconds(30),
        )
        .unwrap();
    let transfer = source.custody_transfer(&claim).unwrap();
    let receipt = receiver
        .accept_custody(
            &authenticate_custody_transfer(
                &mut WitnessAuthenticator::new(&source_policy.local_machine_ref, &"1".repeat(64)),
                transfer,
            )
            .unwrap(),
            &authority_policy.local_machine_ref,
            &authority_policy.local_incarnation_ref,
        )
        .unwrap();
    source
        .acknowledge_remote_custody(
            &claim,
            &authenticate_custody_receipt(
                &mut WitnessAuthenticator::new(
                    &authority_policy.local_machine_ref,
                    &"1".repeat(64),
                ),
                &authority_policy.local_machine_ref,
                receipt,
            )
            .unwrap(),
        )
        .unwrap();
    message_id
}

#[test]
#[allow(clippy::too_many_lines)] // One Gate0B.3 proof follows the complete production transport boundary.
fn gate_0b_3_production_transport_accepts_dynamic_current_lease_proof() {
    let (source_dir, source, fence, _holder, _delivery, _receipt) = ownership_lease_fixture();
    let authority_dir = tempfile::tempdir().unwrap();
    let receiver = WorkLedger::open(authority_dir.path()).unwrap();
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let message_id = gate_0b_3_stage_and_accept_ordinary_custody(
        source_dir.path(),
        &source,
        &receiver,
        &source_policy,
        &authority_policy,
        &fence,
        "Gate0B.3 production dynamic lease proof",
    );

    let (_object, initial_material, initial_holder) = source
        .ownership_holder_material(&fence.work_item_id, &fence.ownership_id, 1)
        .unwrap();
    let initial = source
        .establish_ownership_lease(
            &fence,
            &initial_holder,
            Utc::now() + ChronoDuration::seconds(60),
        )
        .unwrap();
    let release_digest = source
        .release_ownership_lease_with_material(
            &fence.ownership_id,
            &initial_material,
            initial.lease_generation,
        )
        .unwrap();
    let proof = serde_json::to_vec(&serde_json::json!({
        "kind": "explicit_release",
        "release_digest": release_digest,
    }))
    .unwrap();
    let (adopted, successor_material) = source
        .adopt_ownership_with_protected_holder(
            &fence.ownership_id,
            initial.lease_generation,
            Utc::now() + ChronoDuration::seconds(60),
            &proof,
            None,
        )
        .unwrap();
    let OwnershipAdoptionResult::SuccessorCreated(successor) = adopted else {
        panic!("expected dynamic successor")
    };
    assert_ne!(
        successor.proof_digest,
        "c".repeat(64),
        "the daemon's legacy static policy proof is not the dynamic current lease proof"
    );
    assert_ne!(
        authority_policy.local_incarnation_ref,
        successor.holder.incarnation_ref
    );
    assert!(
        source
            .prepare_custody_successor_rebind_with_holder(
                &message_id,
                &authority_policy.local_incarnation_ref,
                &authority_policy.local_incarnation_ref,
                &authority_policy.local_route_ref,
                &authority_policy.local_terminal_adapter,
                &authority_policy.authority_digest,
                &fence.ownership_id,
                successor.lease_generation,
                &initial_material,
            )
            .is_err(),
        "the predecessor holder session cannot authorize adopted custody"
    );
    let rebind = source
        .prepare_custody_successor_rebind_with_holder(
            &message_id,
            &authority_policy.local_incarnation_ref,
            &authority_policy.local_incarnation_ref,
            &authority_policy.local_route_ref,
            &authority_policy.local_terminal_adapter,
            &authority_policy.authority_digest,
            &fence.ownership_id,
            successor.lease_generation,
            &successor_material,
        )
        .unwrap();
    assert_eq!(
        rebind.old_target_incarnation_ref, rebind.new_target_incarnation_ref,
        "adoption does not rewrite the custody daemon endpoint"
    );
    assert_eq!(rebind.successor_holder_ref, successor.holder.holder_ref);
    assert_eq!(
        rebind.successor_session_incarnation_ref,
        successor.holder.incarnation_ref
    );
    let mut successor_policy = authority_policy.clone();
    let response = deliver_request(
        &successor_policy,
        authority_dir.path(),
        &source_policy.local_machine_ref,
        &CustodyTransportRequest::SuccessorRebind {
            schema_version: SUCCESSOR_SCHEMA_VERSION,
            rebind: rebind.clone(),
        },
    );
    let CustodyTransportResponse::SuccessorPrepared { receipt, .. } = response else {
        panic!("dynamic current lease proof was refused")
    };
    gate_0b_3_stage_and_accept_ordinary_custody(
        source_dir.path(),
        &source,
        &receiver,
        &source_policy,
        &authority_policy,
        &fence,
        "Gate0B.3 ordinary transfer after adopted holder",
    );
    source
        .acknowledge_custody_successor_rebind(
            &authenticate_custody_successor_receipt(
                &mut WitnessAuthenticator::new(
                    &authority_policy.local_machine_ref,
                    &"1".repeat(64),
                ),
                &authority_policy.local_machine_ref,
                receipt.clone(),
            )
            .unwrap(),
        )
        .unwrap();
    successor_policy.local_incarnation_ref = opaque("incarnation", "Gate0B.3 post-ack replacement");
    successor_policy.local_route_ref = opaque("route", "Gate0B.3 post-ack replacement");
    successor_policy.local_terminal_adapter = "post-ack-replacement".to_owned();
    successor_policy.authority_digest = sha256(b"Gate0B.3 post-ack replacement");
    let mut replacement_carrier = LoopbackCarrier::new(
        &successor_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    let source_rebind_state = || -> String {
        rusqlite::Connection::open(WorkLedger::path_at(source_dir.path()))
            .unwrap()
            .query_row(
                "SELECT state FROM custody_successor_rebinds
                  WHERE rebind_id = ?1 AND side = 'sender'",
                [&rebind.rebind_id],
                |row| row.get(0),
            )
            .unwrap()
    };
    for refused_version in [1, 3] {
        replacement_carrier.successor_response_schema_override =
            Some(SuccessorResponseSchemaOverride::Prepared(refused_version));
        assert!(
            reconcile_once(&source_policy, source_dir.path(), &mut replacement_carrier).is_err()
        );
        assert_eq!(source_rebind_state(), "acknowledged");
    }
    for refused_version in [1, 3] {
        replacement_carrier.successor_response_schema_override =
            Some(SuccessorResponseSchemaOverride::Finalized(refused_version));
        assert!(
            reconcile_once(&source_policy, source_dir.path(), &mut replacement_carrier).is_err()
        );
        assert_eq!(source_rebind_state(), "acknowledged");
    }
    replacement_carrier.successor_response_schema_override = None;
    reconcile_once(&source_policy, source_dir.path(), &mut replacement_carrier)
        .expect("post-ack source restart replays prepare and finalizes against replacement");
    let source_state: String = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path()))
        .unwrap()
        .query_row(
            "SELECT state FROM custody_successor_rebinds
              WHERE rebind_id = ?1 AND side = 'sender'",
            [&rebind.rebind_id],
            |row| row.get(0),
        )
        .unwrap();
    let receiver_state: String =
        rusqlite::Connection::open(WorkLedger::path_at(authority_dir.path()))
            .unwrap()
            .query_row(
                "SELECT state FROM custody_successor_rebinds
                  WHERE rebind_id = ?1 AND side = 'receiver'",
                [&rebind.rebind_id],
                |row| row.get(0),
            )
            .unwrap();
    assert_eq!(
        (source_state.as_str(), receiver_state.as_str()),
        ("finalized", "committed")
    );
}

#[test]
fn gate_0b_3_successor_wire_v2_refuses_both_rolling_upgrade_directions_safely() {
    #[derive(serde::Deserialize)]
    #[allow(dead_code)] // Deserialization itself is the legacy rolling-upgrade oracle.
    #[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
    enum LegacyRequest {
        SuccessorRebind {
            schema_version: u32,
            rebind: serde_json::Value,
        },
    }

    let authority_dir = tempfile::tempdir().unwrap();
    WorkLedger::open(authority_dir.path()).unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let old_request = serde_json::json!({
        "operation": "successor_rebind",
        "schema_version": 1,
        "rebind": {},
    });
    let parsed: CustodyTransportRequest = serde_json::from_value(old_request).unwrap();
    assert!(matches!(
        deliver_request(
            &authority_policy,
            authority_dir.path(),
            &source_policy.local_machine_ref,
            &parsed,
        ),
        CustodyTransportResponse::Refused { ref reason_code, .. }
            if reason_code == "custody-successor-schema-version-refused"
    ));

    let v2_operation = serde_json::json!({
        "operation": "successor_rebind_v2",
        "schema_version": 2,
        "rebind": {},
    });
    assert!(serde_json::from_value::<LegacyRequest>(v2_operation).is_err());
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
struct Gate0B3FilesystemEntry {
    bytes: Option<Vec<u8>>,
    mode: u32,
    modified: std::time::SystemTime,
}

#[cfg(unix)]
fn gate_0b_3_filesystem_snapshot(root: &Path) -> BTreeMap<PathBuf, Gate0B3FilesystemEntry> {
    fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<PathBuf, Gate0B3FilesystemEntry>) {
        let metadata = fs::symlink_metadata(path).unwrap();
        snapshot.insert(
            path.strip_prefix(root).unwrap().to_path_buf(),
            Gate0B3FilesystemEntry {
                bytes: metadata.is_file().then(|| fs::read(path).unwrap()),
                mode: metadata.permissions().mode() & 0o777,
                modified: metadata.modified().unwrap(),
            },
        );
        if metadata.is_dir() {
            let mut children = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            children.sort();
            for child in children {
                visit(root, &child, snapshot);
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)] // One Gate0B.3 proof snapshots every protected persistence surface.
fn gate_0b_3_incompatible_successor_wire_refuses_before_opening_protected_ledger() {
    let state_dir = tempfile::tempdir().unwrap();
    seed_native_wake(state_dir.path(), 146);
    let policy = test_policy(state_dir.path(), true);
    let evidence = IncomingPeerEvidence {
        peer_machine_ref: policy.peers.keys().next().unwrap().clone(),
        ssh_connection_present: true,
        ssh_auth_key_sha256: policy
            .peers
            .values()
            .next()
            .unwrap()
            .ssh_auth_key_sha256
            .clone(),
    };
    let database = WorkLedger::path_at(state_dir.path());
    let live_connection = rusqlite::Connection::open(&database).unwrap();
    live_connection
        .execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
        .unwrap();
    live_connection
        .execute(
            "UPDATE repo_policies SET revision = revision + 1 WHERE repo = 'owner/repo'",
            [],
        )
        .unwrap();
    let wal = database.with_extension("sqlite3-wal");
    let shm = database.with_extension("sqlite3-shm");
    assert!(wal.is_file() && shm.is_file());
    for path in [&database, &wal, &shm] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let object_directory = state_dir.path().join("work-ledger/protected-objects");
    fs::create_dir_all(&object_directory).unwrap();
    fs::set_permissions(&object_directory, fs::Permissions::from_mode(0o700)).unwrap();
    let pending = object_directory.join(".pending-Gate0B.3-incompatible-successor");
    fs::write(&pending, b"Gate0B.3 pending protected object").unwrap();
    fs::set_permissions(&pending, fs::Permissions::from_mode(0o600)).unwrap();

    let ledger_root = state_dir.path().join("work-ledger");
    let before = gate_0b_3_filesystem_snapshot(&ledger_root);
    let incompatible = [
        serde_json::json!({
            "operation": "successor_rebind",
            "schema_version": 1,
            "rebind": {},
        }),
        serde_json::json!({
            "operation": "successor_rebind_v2",
            "schema_version": 3,
            "rebind": {
                "rebind_id": "cr_Gate0B.3-future",
                "message_id": "wm_Gate0B.3-future",
                "identity_digest": "0".repeat(64),
                "workstream_revision": 1,
                "source_machine_ref": "machine_Gate0B.3-source",
                "target_machine_ref": policy.local_machine_ref,
                "old_target_incarnation_ref": policy.local_incarnation_ref,
                "new_target_incarnation_ref": policy.local_incarnation_ref,
                "old_authority_epoch": 1,
                "new_authority_epoch": 2,
                "old_transfer_digest": "1".repeat(64),
                "old_custody_receipt_digest": "2".repeat(64),
                "new_target_route_ref": policy.local_route_ref,
                "terminal_adapter": policy.local_terminal_adapter,
                "new_authority_digest": policy.authority_digest,
                "ownership_lease_id": "ol_Gate0B.3-future",
                "ownership_lease_generation": 2,
                "ownership_lease_expires_at": "2026-08-31T23:59:59Z",
                "ownership_root_uuid": "00000000-0000-0000-0000-000000000001",
                "repository_provider": "github.com",
                "repository_id": "R_Gate0B.3",
                "repository": "owner/repo",
                "pull_request": 146,
                "exact_head": "3".repeat(40),
                "workstream_handle": "GEN-37",
                "successor_holder_ref": "owner_Gate0B.3-successor",
                "successor_session_incarnation_ref": "incarnation_Gate0B.3-successor",
                "successor_proof_digest": "4".repeat(64),
                "rebind_digest": "5".repeat(64),
            },
        }),
    ];
    for request in incompatible {
        let response = handle_incoming_request(
            &policy,
            state_dir.path(),
            &evidence,
            &serde_json::to_vec(&request).unwrap(),
        );
        assert!(matches!(
            response,
            CustodyTransportResponse::Refused { ref reason_code, .. }
                if reason_code == "custody-successor-schema-version-refused"
        ));
        assert_eq!(
            gate_0b_3_filesystem_snapshot(&ledger_root),
            before,
            "schema refusal must precede DB/WAL/SHM or protected-object reconciliation"
        );
        assert!(pending.is_file());
    }
    drop(live_connection);
}

#[test]
fn old_incarnation_and_authority_contradiction_refuse_without_effect() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 45);
    seed_native_wake(authority_dir.path(), 45);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let mut to_authority = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();
    let request = to_authority.last_request.clone().unwrap();

    let mut replaced = authority_policy.clone();
    replaced.local_incarnation_ref = opaque("incarnation", "authority-replaced");
    let response = deliver_request(
        &replaced,
        authority_dir.path(),
        &source_policy.local_machine_ref,
        &request,
    );
    assert!(matches!(response, CustodyTransportResponse::Refused { .. }));

    let mut contradictory = authority_policy.clone();
    contradictory.authority_digest = "f".repeat(64);
    let response = deliver_request(
        &contradictory,
        authority_dir.path(),
        &source_policy.local_machine_ref,
        &request,
    );
    assert!(matches!(response, CustodyTransportResponse::Refused { .. }));
    let authority = WorkLedger::open_existing(authority_dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(authority.custody_status().unwrap().incoming_processed, 0);
}

#[test]
fn one_failed_record_does_not_starve_a_later_native_obligation() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 46);
    seed_native_wake(source_dir.path(), 47);
    seed_native_wake(authority_dir.path(), 46);
    seed_native_wake(authority_dir.path(), 47);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let mut carrier = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    carrier.refuse_first_transfer = true;
    assert!(reconcile_once(&source_policy, source_dir.path(), &mut carrier).is_err());
    let source = WorkLedger::open_existing(source_dir.path())
        .unwrap()
        .unwrap();
    let status = source.custody_status().unwrap();
    assert_eq!(status.outgoing_accepted, 1);
    assert_eq!(status.outgoing_claimed, 1);
}

#[test]
fn contradictory_local_publication_never_commits_a_processed_effect() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 48);
    seed_native_wake_with_plan(authority_dir.path(), 48, sha256(b"different-plan"));
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let mut to_authority = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();

    let mut to_source = LoopbackCarrier::new(
        &source_policy,
        source_dir.path(),
        authority_policy.local_machine_ref.clone(),
    );
    assert!(reconcile_once(&authority_policy, authority_dir.path(), &mut to_source).is_err());
    let source = WorkLedger::open_existing(source_dir.path())
        .unwrap()
        .unwrap();
    let authority = WorkLedger::open_existing(authority_dir.path())
        .unwrap()
        .unwrap();
    assert_eq!(source.custody_status().unwrap().outgoing_accepted, 1);
    assert_eq!(authority.custody_status().unwrap().incoming_processing, 1);
    assert_eq!(authority.custody_status().unwrap().incoming_processed, 0);
}

#[test]
#[allow(clippy::too_many_lines)] // One planted lifecycle table proves every outcome and zero writes.
fn custody_inventory_maps_states_without_guessing_a_route() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 61);
    seed_native_wake(authority_dir.path(), 61);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    stage_native_obligations(
        &source_policy,
        &WorkLedger::open_existing(source_dir.path())
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    let message_id: String = connection
        .query_row("SELECT message_id FROM custody_outbox", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(connection);

    let mut never_called = RefusingCarrier;
    assert!(matches!(
        custody_inventory_with_carrier(&source_policy, source_dir.path(), &message_id, &mut never_called),
        CustodyInventoryResult::Uncertain { ref reason_code, .. }
            if reason_code == "custody-not-yet-accepted"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET custody_receipt_digest = ?2 WHERE message_id = ?1",
            rusqlite::params![message_id, "9".repeat(64)],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(&source_policy, source_dir.path(), &message_id, &mut never_called),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-state-contradictory"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET custody_receipt_digest = NULL WHERE message_id = ?1",
            [&message_id],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(
            &source_policy,
            source_dir.path(),
            &format!("wm_{}", "0".repeat(64)),
            &mut never_called,
        ),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-message-missing-or-contradictory"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET state = 'claimed' WHERE message_id = ?1",
            [&message_id],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(&source_policy, source_dir.path(), &message_id, &mut never_called),
        CustodyInventoryResult::Uncertain { ref reason_code, .. }
            if reason_code == "custody-not-yet-accepted"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET state = 'superseded' WHERE message_id = ?1",
            [&message_id],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(&source_policy, source_dir.path(), &message_id, &mut never_called),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-message-terminally-invalid"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET state = 'pending' WHERE message_id = ?1",
            [&message_id],
        )
        .unwrap();
    drop(connection);

    let mut to_authority = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();
    let source_before = filesystem_snapshot(source_dir.path());
    let authority_before = filesystem_snapshot(authority_dir.path());
    let result = custody_inventory_with_carrier(
        &source_policy,
        source_dir.path(),
        &message_id,
        &mut to_authority,
    );
    let CustodyInventoryResult::Complete { inventory, .. } = result else {
        panic!("accepted custody must query the exact receiver");
    };
    assert_eq!(inventory.items.len(), 1);
    assert_eq!(inventory.items[0].repository, "owner/repo");
    assert_eq!(inventory.items[0].pull_request, 61);
    assert_eq!(filesystem_snapshot(source_dir.path()), source_before);
    assert_eq!(filesystem_snapshot(authority_dir.path()), authority_before);
    assert!(matches!(
        custody_inventory_with_carrier(
            &source_policy,
            source_dir.path(),
            &message_id,
            &mut never_called,
        ),
        CustodyInventoryResult::Uncertain { ref reason_code, .. }
            if reason_code == "custody-peer-unavailable"
    ));

    let authority_connection =
        rusqlite::Connection::open(WorkLedger::path_at(authority_dir.path())).unwrap();
    authority_connection
        .execute_batch(
            "DROP TRIGGER workstream_projection_binding_identity_immutable;
             DROP TRIGGER workstream_projection_binding_repository_identity_enrichment;",
        )
        .unwrap();
    authority_connection
        .execute(
            "UPDATE workstream_projection_bindings
                SET repository_provider = NULL, repository_id = NULL",
            [],
        )
        .unwrap();
    drop(authority_connection);
    assert!(matches!(
        custody_inventory_with_carrier(
            &source_policy,
            source_dir.path(),
            &message_id,
            &mut to_authority,
        ),
        CustodyInventoryResult::Partial { ref inventory, .. }
            if !inventory.complete && !inventory.truncated
    ));

    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET state = 'processed', processed_receipt_digest = ?2
              WHERE message_id = ?1",
            rusqlite::params![message_id, "8".repeat(64)],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(
            &source_policy,
            source_dir.path(),
            &message_id,
            &mut to_authority
        ),
        CustodyInventoryResult::Partial { .. }
    ));

    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET state = 'cancelled' WHERE message_id = ?1",
            [&message_id],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(&source_policy, source_dir.path(), &message_id, &mut never_called),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-message-terminally-invalid"
    ));
}

#[cfg(unix)]
#[test]
fn custody_inventory_uses_the_pinned_fixed_argv_ssh_subsystem() {
    use std::os::unix::fs::PermissionsExt as _;

    let temp = tempfile::tempdir().unwrap();
    let argv = temp.path().join("argv");
    let environment = temp.path().join("environment");
    let script = temp.path().join("fake-ssh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\n/usr/bin/env | /usr/bin/sort > '{}'\nprintf '{{}}'\n",
            argv.display(),
            environment.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let identity = temp.path().join("identity");
    std::fs::write(&identity, b"private").unwrap();
    std::fs::set_permissions(&identity, std::fs::Permissions::from_mode(0o600)).unwrap();
    let known_hosts = temp.path().join("known-hosts");
    std::fs::write(&known_hosts, b"peer ssh-ed25519 AAAA\n").unwrap();
    std::fs::set_permissions(&known_hosts, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut peer = peer(
        temp.path(),
        &opaque("machine", "ssh-peer"),
        &opaque("incarnation", "ssh-peer"),
        "a".repeat(64),
    );
    peer.ssh_program = script;
    peer.identity_file = identity;
    peer.known_hosts_file = known_hosts;
    peer.destination = "custody@example.invalid".to_owned();
    peer.port = 22022;
    peer.remote_subsystem = "shipyard-custody-v1".to_owned();
    let request = CustodyTransportRequest::Inventory {
        schema_version: SCHEMA_VERSION,
        request: CustodyInventoryWireRequest::new(crate::work_ledger::CustodyInventoryBinding {
            message_id: format!("wm_{}", "1".repeat(64)),
            identity_digest: "2".repeat(64),
            source_machine_ref: opaque("machine", "ssh-source"),
            source_incarnation_ref: opaque("incarnation", "ssh-source"),
            target_machine_ref: peer.machine_ref.clone(),
            target_incarnation_ref: peer.incarnation_ref.clone(),
            target_route_ref: peer.route_ref.clone(),
            terminal_adapter: peer.terminal_adapter.clone(),
            rebind_epoch: 1,
            authority_digest: "3".repeat(64),
            transfer_digest: "4".repeat(64),
        })
        .unwrap(),
    };
    let error = SshCustodyCarrier
        .exchange(
            &peer,
            &request,
            Instant::now() + Duration::from_secs(5),
            4096,
        )
        .unwrap_err();
    assert!(
        matches!(
            &error,
            CustodyCarrierError::Refused(reason) if reason == "custody-response-malformed"
        ),
        "unexpected carrier error: {error:?}"
    );
    let argv = std::fs::read_to_string(argv).unwrap();
    assert!(argv.contains("-F\n/dev/null\n"));
    assert!(argv.contains("-s\n--\ncustody@example.invalid\nshipyard-custody-v1\n"));
    assert!(!argv.contains("sh -c"));
    let environment = std::fs::read_to_string(environment).unwrap();
    assert!(environment.contains("SHIPYARD_CUSTODY_KNOWN_HOSTS="));
    assert!(environment.contains("LANG=C\n"));
    assert!(!environment.contains("HOME="));
    assert!(!environment.contains("SSH_AUTH_SOCK="));

    std::fs::write(&peer.ssh_program, "#!/bin/sh\nexit 255\n").unwrap();
    std::fs::set_permissions(&peer.ssh_program, std::fs::Permissions::from_mode(0o700)).unwrap();
    let error = SshCustodyCarrier
        .exchange(
            &peer,
            &request,
            Instant::now() + Duration::from_secs(5),
            4096,
        )
        .unwrap_err();
    assert!(matches!(error, CustodyCarrierError::Unavailable(_)));
}

#[test]
fn custody_inventory_refuses_tuple_and_response_identity_mutations() {
    let source_dir = tempfile::tempdir().unwrap();
    let authority_dir = tempfile::tempdir().unwrap();
    seed_native_wake(source_dir.path(), 62);
    seed_native_wake(authority_dir.path(), 62);
    let (source_policy, authority_policy) = policy_pair(source_dir.path(), authority_dir.path());
    let mut to_authority = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    reconcile_once(&source_policy, source_dir.path(), &mut to_authority).unwrap();
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    let message_id: String = connection
        .query_row("SELECT message_id FROM custody_outbox", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(connection);
    let mut raced = RebindingAfterResponseCarrier {
        inner: LoopbackCarrier::new(
            &authority_policy,
            authority_dir.path(),
            source_policy.local_machine_ref.clone(),
        ),
        source_state_dir: source_dir.path(),
    };
    assert!(matches!(
        custody_inventory_with_carrier(
            &source_policy,
            source_dir.path(),
            &message_id,
            &mut raced,
        ),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-inventory-response-refused"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute(
            "UPDATE custody_outbox SET active_rebind_epoch = 1 WHERE message_id = ?1",
            [&message_id],
        )
        .unwrap();
    connection
        .execute_batch("DROP TRIGGER custody_rebind_no_delete;")
        .unwrap();
    connection
        .execute(
            "DELETE FROM custody_rebinds WHERE message_id = ?1 AND epoch = 2",
            [&message_id],
        )
        .unwrap();
    drop(connection);
    let mut mutated_response = LoopbackCarrier::new(
        &authority_policy,
        authority_dir.path(),
        source_policy.local_machine_ref.clone(),
    );
    mutated_response.mutate_inventory_response = true;
    assert!(matches!(
        custody_inventory_with_carrier(
            &source_policy,
            source_dir.path(),
            &message_id,
            &mut mutated_response,
        ),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-inventory-response-refused"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(source_dir.path())).unwrap();
    connection
        .execute_batch("DROP TRIGGER custody_rebind_immutable;")
        .unwrap();
    connection
        .execute(
            "UPDATE custody_rebinds SET target_route_ref = ?2 WHERE message_id = ?1",
            rusqlite::params![message_id, opaque("route", "mutated")],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        custody_inventory_with_carrier(&source_policy, source_dir.path(), &message_id, &mut to_authority),
        CustodyInventoryResult::Refused { ref reason_code, .. }
            if reason_code == "custody-inventory-policy-route-mismatch"
    ));
}

#[test]
fn custody_inventory_preserves_equal_pr_numbers_across_repositories() {
    let state = tempfile::tempdir().unwrap();
    let ledger = WorkLedger::open(state.path()).unwrap();
    seed_native_wake(state.path(), 63);
    let mut request = native_publication_test_request();
    request.repository = "another/repo".to_owned();
    request.pull_request = 63;
    request.head_sha = "f".repeat(40);
    request.workstream_handle = "GEN-6300".to_owned();
    request.repository_id = "R_another".to_owned();
    ledger
        .set_repo_policy(
            &RepoPolicy {
                repo: request.repository.clone(),
                primary_platform: "macos".to_owned(),
                compatibility_mode: "independent".to_owned(),
                compatibility_lanes: Vec::new(),
                blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                declared_dependency_lanes: Vec::new(),
                revision: 0,
            },
            0,
        )
        .unwrap();
    let policy =
        native_publication_test_policy(vec!["owner/repo".to_owned(), request.repository.clone()]);
    ledger
        .publish_native_continuation(&request, &policy, true)
        .unwrap();
    let inventory = crate::work_ledger::local_work_inventory(state.path()).unwrap();
    assert_eq!(
        inventory
            .items
            .iter()
            .filter(|item| item.pull_request == 63)
            .count(),
        2
    );
    assert_ne!(inventory.items[0].repository, inventory.items[1].repository);
}

fn test_policy(root: &Path, local_is_authority: bool) -> CustodyTransportPolicy {
    let local = opaque("machine", "local");
    let peer = opaque("machine", "peer");
    let peer_policy = CustodyPeer {
        machine_ref: peer.clone(),
        incarnation_ref: opaque("incarnation", "peer"),
        route_ref: opaque("route", "peer"),
        terminal_adapter: "cmux".to_owned(),
        ssh_program: "/usr/bin/ssh".into(),
        destination: "peer".to_owned(),
        known_hosts_file: root.join("known-hosts"),
        identity_file: root.join("identity"),
        port: 22,
        remote_subsystem: "shipyard-custody-v1".to_owned(),
        ssh_auth_key_sha256: "b".repeat(64),
    };
    CustodyTransportPolicy {
        local_machine_ref: local.clone(),
        local_incarnation_ref: opaque("incarnation", "local"),
        local_route_ref: opaque("route", "local"),
        local_terminal_adapter: "cmux".to_owned(),
        mutation_authority_machine_ref: if local_is_authority {
            local
        } else {
            peer.clone()
        },
        authority_digest: "d".repeat(64),
        sender_owner_ref: opaque("owner", "sender"),
        inbox_owner_ref: opaque("owner", "inbox"),
        lease_seconds: 30,
        deadline_seconds: 5,
        max_output_bytes: 4096,
        peers: BTreeMap::from([(peer, peer_policy)]),
        policy_digest: "e".repeat(64),
    }
}

fn policy_pair(
    source_root: &Path,
    authority_root: &Path,
) -> (CustodyTransportPolicy, CustodyTransportPolicy) {
    let source_machine = opaque("machine", "source");
    let authority_machine = opaque("machine", "authority");
    let source_incarnation = opaque("incarnation", "source");
    let authority_incarnation = opaque("incarnation", "authority");
    let authority_digest = "d".repeat(64);
    let source_peer = peer(
        source_root,
        &source_machine,
        &source_incarnation,
        "a".repeat(64),
    );
    let authority_peer = peer(
        authority_root,
        &authority_machine,
        &authority_incarnation,
        "b".repeat(64),
    );
    let source = CustodyTransportPolicy {
        local_machine_ref: source_machine.clone(),
        local_incarnation_ref: source_incarnation,
        local_route_ref: opaque("route", &source_machine),
        local_terminal_adapter: "cmux".to_owned(),
        mutation_authority_machine_ref: authority_machine.clone(),
        authority_digest: authority_digest.clone(),
        sender_owner_ref: opaque("owner", "source-sender"),
        inbox_owner_ref: opaque("owner", "source-inbox"),
        lease_seconds: 30,
        deadline_seconds: 5,
        max_output_bytes: 4096,
        peers: BTreeMap::from([(authority_machine.clone(), authority_peer)]),
        policy_digest: "e".repeat(64),
    };
    let authority = CustodyTransportPolicy {
        local_machine_ref: authority_machine,
        local_incarnation_ref: authority_incarnation,
        local_route_ref: opaque("route", &source.mutation_authority_machine_ref),
        local_terminal_adapter: "cmux".to_owned(),
        mutation_authority_machine_ref: source.mutation_authority_machine_ref.clone(),
        authority_digest,
        sender_owner_ref: opaque("owner", "authority-sender"),
        inbox_owner_ref: opaque("owner", "authority-inbox"),
        lease_seconds: 30,
        deadline_seconds: 5,
        max_output_bytes: 4096,
        peers: BTreeMap::from([(source_machine, source_peer)]),
        policy_digest: "f".repeat(64),
    };
    (source, authority)
}

fn peer(root: &Path, machine: &str, incarnation: &str, key: String) -> CustodyPeer {
    CustodyPeer {
        machine_ref: machine.to_owned(),
        incarnation_ref: incarnation.to_owned(),
        route_ref: opaque("route", machine),
        terminal_adapter: "cmux".to_owned(),
        ssh_program: "/usr/bin/ssh".into(),
        destination: "peer".to_owned(),
        known_hosts_file: root.join("known-hosts"),
        identity_file: root.join("identity"),
        port: 22,
        remote_subsystem: "shipyard-custody-v1".to_owned(),
        ssh_auth_key_sha256: key,
    }
}

fn seed_native_wake(state_dir: &Path, pull_request: u64) {
    seed_native_wake_with_plan(state_dir, pull_request, sha256(b"GEN-43-plan"));
}

fn seed_native_wake_with_plan(state_dir: &Path, pull_request: u64, plan_sha256: String) {
    let ledger = WorkLedger::open(state_dir).unwrap();
    if ledger.repo_policy("owner/repo").unwrap().is_none() {
        ledger
            .set_repo_policy(
                &RepoPolicy {
                    repo: "owner/repo".to_owned(),
                    primary_platform: "macos".to_owned(),
                    compatibility_mode: "independent".to_owned(),
                    compatibility_lanes: Vec::new(),
                    blocking_rule: "declared_dependency_or_shared_integrity".to_owned(),
                    declared_dependency_lanes: Vec::new(),
                    revision: 0,
                },
                0,
            )
            .unwrap();
    }
    let mut request = native_publication_test_request();
    request.pull_request = pull_request;
    request.head_sha = format!("{pull_request:040x}");
    request.workstream_handle = format!("GEN-{pull_request}");
    request.plan_sha256 = plan_sha256;
    let policy = native_publication_test_policy(vec![request.repository.clone()]);
    ledger
        .publish_native_continuation(&request, &policy, true)
        .unwrap();
    ledger
        .apply_native_steward_disposition(
            &request.repository,
            request.pull_request,
            &request.head_sha,
            NativeStewardDisposition::Actionable,
        )
        .unwrap();
}

struct RefusingCarrier;

impl CustodyCarrier for RefusingCarrier {
    fn exchange(
        &mut self,
        _peer: &CustodyPeer,
        _request: &CustodyTransportRequest,
        _deadline: Instant,
        _max_output_bytes: u64,
    ) -> Result<(CustodyTransportResponse, String), CustodyCarrierError> {
        Err(CustodyCarrierError::Unavailable(
            "custody-peer-unavailable".to_owned(),
        ))
    }
}

struct LoopbackCarrier<'a> {
    remote_policy: &'a CustodyTransportPolicy,
    remote_state_dir: &'a Path,
    source_machine_ref: String,
    refuse_first_transfer: bool,
    refused_transfer: bool,
    last_request: Option<CustodyTransportRequest>,
    successor_response_schema_override: Option<SuccessorResponseSchemaOverride>,
    mutate_inventory_response: bool,
}

#[derive(Clone, Copy)]
enum SuccessorResponseSchemaOverride {
    Prepared(u32),
    Finalized(u32),
    Aborted(u32),
}

struct RebindingAfterResponseCarrier<'a> {
    inner: LoopbackCarrier<'a>,
    source_state_dir: &'a Path,
}

impl CustodyCarrier for RebindingAfterResponseCarrier<'_> {
    fn exchange(
        &mut self,
        peer: &CustodyPeer,
        request: &CustodyTransportRequest,
        deadline: Instant,
        max_output_bytes: u64,
    ) -> Result<(CustodyTransportResponse, String), CustodyCarrierError> {
        let response = self
            .inner
            .exchange(peer, request, deadline, max_output_bytes)?;
        let connection =
            rusqlite::Connection::open(WorkLedger::path_at(self.source_state_dir)).unwrap();
        connection
            .execute(
                "INSERT INTO custody_rebinds
             SELECT message_id, 2, target_machine_ref, target_incarnation_ref, ?2,
                    terminal_adapter, authority_digest, created_at
               FROM custody_rebinds WHERE message_id = ?1 AND epoch = 1",
                rusqlite::params![request_message_id(request), opaque("route", "raced")],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE custody_outbox SET active_rebind_epoch = 2 WHERE message_id = ?1",
                [request_message_id(request)],
            )
            .unwrap();
        Ok(response)
    }
}

fn request_message_id(request: &CustodyTransportRequest) -> &str {
    match request {
        CustodyTransportRequest::Inventory { request, .. } => &request.binding.message_id,
        _ => panic!("expected inventory request"),
    }
}

impl<'a> LoopbackCarrier<'a> {
    fn new(
        remote_policy: &'a CustodyTransportPolicy,
        remote_state_dir: &'a Path,
        source_machine_ref: String,
    ) -> Self {
        Self {
            remote_policy,
            remote_state_dir,
            source_machine_ref,
            refuse_first_transfer: false,
            refused_transfer: false,
            last_request: None,
            successor_response_schema_override: None,
            mutate_inventory_response: false,
        }
    }
}

impl CustodyCarrier for LoopbackCarrier<'_> {
    fn exchange(
        &mut self,
        _peer: &CustodyPeer,
        request: &CustodyTransportRequest,
        _deadline: Instant,
        _max_output_bytes: u64,
    ) -> Result<(CustodyTransportResponse, String), CustodyCarrierError> {
        self.last_request = Some(request.clone());
        if self.refuse_first_transfer
            && !self.refused_transfer
            && matches!(request, CustodyTransportRequest::Transfer { .. })
        {
            self.refused_transfer = true;
            return Err(CustodyCarrierError::Unavailable(
                "custody-peer-unavailable".to_owned(),
            ));
        }
        let mut response = deliver_request(
            self.remote_policy,
            self.remote_state_dir,
            &self.source_machine_ref,
            request,
        );
        match (self.successor_response_schema_override, &mut response) {
            (
                Some(SuccessorResponseSchemaOverride::Prepared(version)),
                CustodyTransportResponse::SuccessorPrepared { schema_version, .. },
            )
            | (
                Some(SuccessorResponseSchemaOverride::Finalized(version)),
                CustodyTransportResponse::SuccessorFinalized { schema_version, .. },
            )
            | (
                Some(SuccessorResponseSchemaOverride::Aborted(version)),
                CustodyTransportResponse::SuccessorAborted { schema_version, .. },
            ) => *schema_version = version,
            _ => {}
        }
        if self.mutate_inventory_response {
            match &mut response {
                CustodyTransportResponse::InventoryComplete {
                    request_digest,
                    inventory,
                    ..
                }
                | CustodyTransportResponse::InventoryPartial {
                    request_digest,
                    inventory,
                    ..
                } => {
                    *request_digest = "9".repeat(64);
                    if let Some(item) = inventory.items.first_mut() {
                        item.exact_head = "A".repeat(40);
                    }
                }
                _ => {}
            }
        }
        Ok((response, "1".repeat(64)))
    }
}

fn deliver_request(
    policy: &CustodyTransportPolicy,
    state_dir: &Path,
    source_machine_ref: &str,
    request: &CustodyTransportRequest,
) -> CustodyTransportResponse {
    let peer = policy.peers.get(source_machine_ref).unwrap();
    handle_incoming_request(
        policy,
        state_dir,
        &IncomingPeerEvidence {
            peer_machine_ref: source_machine_ref.to_owned(),
            ssh_connection_present: true,
            ssh_auth_key_sha256: peer.ssh_auth_key_sha256.clone(),
        },
        &serde_json::to_vec(request).unwrap(),
    )
}

fn expire_active_claim(state_dir: &Path, table: &str) {
    assert!(matches!(
        table,
        "custody_sender_claims" | "custody_inbox_claims"
    ));
    let connection = rusqlite::Connection::open(WorkLedger::path_at(state_dir)).unwrap();
    connection
        .execute(
            &format!("UPDATE {table} SET expires_at = ?1 WHERE state = 'active'"),
            ["2000-01-01T00:00:00Z"],
        )
        .unwrap();
}

fn opaque(prefix: &str, seed: &str) -> String {
    format!("{prefix}_{}", hex::encode(Sha256::digest(seed.as_bytes())))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn filesystem_snapshot(root: &Path) -> Vec<(String, u64, Option<std::time::SystemTime>, String)> {
    fn visit(
        root: &Path,
        path: &Path,
        rows: &mut Vec<(String, u64, Option<std::time::SystemTime>, String)>,
    ) {
        let mut entries = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        for entry in entries {
            let metadata = std::fs::symlink_metadata(&entry).unwrap();
            let relative = entry
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if metadata.is_dir() {
                rows.push((
                    relative,
                    0,
                    metadata.modified().ok(),
                    "directory".to_owned(),
                ));
                visit(root, &entry, rows);
            } else {
                let content = std::fs::read(&entry).unwrap();
                rows.push((
                    relative,
                    metadata.len(),
                    metadata.modified().ok(),
                    sha256(&content),
                ));
            }
        }
    }
    let mut rows = Vec::new();
    visit(root, root, &mut rows);
    rows
}
