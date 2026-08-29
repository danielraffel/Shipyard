use std::collections::BTreeMap;

use super::*;
use crate::work_ledger::{
    NativeStewardDisposition, RepoPolicy, WakeConsumerPolicy, WorkLedger,
    native_publication_test_policy, native_publication_test_request,
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
        successor_proof_digest: "c".repeat(64),
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
        successor_proof_digest: "c".repeat(64),
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
    ) -> Result<(CustodyTransportResponse, String), String> {
        Err("custody-peer-unavailable".to_owned())
    }
}

struct LoopbackCarrier<'a> {
    remote_policy: &'a CustodyTransportPolicy,
    remote_state_dir: &'a Path,
    source_machine_ref: String,
    refuse_first_transfer: bool,
    refused_transfer: bool,
    last_request: Option<CustodyTransportRequest>,
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
    ) -> Result<(CustodyTransportResponse, String), String> {
        self.last_request = Some(request.clone());
        if self.refuse_first_transfer
            && !self.refused_transfer
            && matches!(request, CustodyTransportRequest::Transfer { .. })
        {
            self.refused_transfer = true;
            return Err("custody-peer-unavailable".to_owned());
        }
        Ok((
            deliver_request(
                self.remote_policy,
                self.remote_state_dir,
                &self.source_machine_ref,
                request,
            ),
            "1".repeat(64),
        ))
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
