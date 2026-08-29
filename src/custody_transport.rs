//! Default-off cross-machine carrier for the durable custody protocol.
//!
//! The transport moves strict JSON over bounded, ambient-config-free SSH. It
//! never shares a ledger or a credential: each host keeps its own WAL database,
//! outbound SSH uses a referenced private identity file, and the receiver binds
//! the request to the public key recorded by `sshd` `ExposeAuthInfo`.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::RuntimeMode;
use crate::work_ledger::{
    CustodyControl, CustodyControlReceipt, CustodyReceipt, CustodySuccessorRebind,
    CustodySuccessorReceipt, CustodyTransfer, CustodyTransportAuthenticator, ProcessedReceipt,
    WorkLedger, WorkLedgerError, WorkLedgerResult, authenticate_custody_control,
    authenticate_custody_control_receipt, authenticate_custody_receipt,
    authenticate_custody_successor_rebind, authenticate_custody_successor_receipt,
    authenticate_custody_transfer, authenticate_processed_receipt,
};

mod policy;

use policy::CustodyPeer;
pub(crate) use policy::{CustodyTransportPolicy, load_custody_transport_policy};

const SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_CUSTODY_WIRE_BYTES: u64 = 1024 * 1024;
const MAX_BATCH: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CustodyTransportRequest {
    Transfer {
        schema_version: u32,
        transfer: CustodyTransfer,
    },
    SuccessorRebind {
        schema_version: u32,
        rebind: CustodySuccessorRebind,
    },
    Control {
        schema_version: u32,
        control: CustodyControl,
    },
    Processed {
        schema_version: u32,
        receipt: ProcessedReceipt,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum CustodyTransportResponse {
    CustodyAccepted {
        schema_version: u32,
        receipt: CustodyReceipt,
    },
    SuccessorCommitted {
        schema_version: u32,
        receipt: CustodySuccessorReceipt,
    },
    ControlApplied {
        schema_version: u32,
        receipt: CustodyControlReceipt,
    },
    ProcessedAcknowledged {
        schema_version: u32,
        receipt_digest: String,
    },
    Retryable {
        schema_version: u32,
        reason_code: String,
    },
    Refused {
        schema_version: u32,
        reason_code: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct IncomingPeerEvidence {
    pub(crate) peer_machine_ref: String,
    pub(crate) ssh_connection_present: bool,
    pub(crate) ssh_auth_key_sha256: String,
}

pub(crate) fn incoming_peer_evidence_from_environment(
    policy: &CustodyTransportPolicy,
) -> Result<IncomingPeerEvidence, String> {
    let connection = std::env::var("SSH_CONNECTION").unwrap_or_default();
    let auth_path = std::env::var_os("SSH_USER_AUTH")
        .ok_or_else(|| "custody-incoming-auth-info-missing".to_owned())?;
    let auth_key = read_exposed_ssh_key(Path::new(&auth_path))?;
    let key_digest = hex::encode(Sha256::digest(auth_key.as_bytes()));
    let mut matches = policy
        .peers
        .values()
        .filter(|peer| peer.ssh_auth_key_sha256 == key_digest);
    let peer = matches
        .next()
        .ok_or_else(|| "custody-incoming-peer-key-unknown".to_owned())?;
    if matches.next().is_some() {
        return Err("custody-incoming-peer-key-ambiguous".to_owned());
    }
    Ok(IncomingPeerEvidence {
        peer_machine_ref: peer.machine_ref.clone(),
        ssh_connection_present: !connection.trim().is_empty(),
        ssh_auth_key_sha256: key_digest,
    })
}

pub(crate) fn handle_incoming_request(
    policy: &CustodyTransportPolicy,
    state_dir: &Path,
    evidence: &IncomingPeerEvidence,
    input: &[u8],
) -> CustodyTransportResponse {
    match handle_incoming_request_inner(policy, state_dir, evidence, input) {
        Ok(response) => response,
        Err(reason_code) => CustodyTransportResponse::Refused {
            schema_version: SCHEMA_VERSION,
            reason_code,
        },
    }
}

#[allow(clippy::too_many_lines)] // One strict schema dispatcher keeps auth before every mutation.
fn handle_incoming_request_inner(
    policy: &CustodyTransportPolicy,
    state_dir: &Path,
    evidence: &IncomingPeerEvidence,
    input: &[u8],
) -> Result<CustodyTransportResponse, String> {
    if input.len() as u64 > MAX_CUSTODY_WIRE_BYTES || !evidence.ssh_connection_present {
        return Err("custody-incoming-untrusted".to_owned());
    }
    let peer = policy
        .peers
        .get(&evidence.peer_machine_ref)
        .ok_or_else(|| "custody-incoming-peer-unknown".to_owned())?;
    if peer.ssh_auth_key_sha256 != evidence.ssh_auth_key_sha256 {
        return Err("custody-incoming-peer-key-mismatch".to_owned());
    }
    let request: CustodyTransportRequest =
        serde_json::from_slice(input).map_err(|_| "custody-request-malformed".to_owned())?;
    let ledger = WorkLedger::open_existing(state_dir)
        .map_err(|_| "custody-ledger-unavailable".to_owned())?
        .ok_or_else(|| "custody-ledger-absent".to_owned())?;
    let mut authenticator = BoundAuthenticator::new(
        &evidence.peer_machine_ref,
        &policy.policy_digest,
        &evidence.ssh_auth_key_sha256,
    );
    match request {
        CustodyTransportRequest::Transfer {
            schema_version,
            transfer,
        } => {
            require_schema(schema_version)?;
            require_local_mutation_authority(policy)?;
            if transfer.rebind_authority_digest != policy.authority_digest {
                return Err("custody-transfer-authority-mismatch".to_owned());
            }
            if transfer.target_route_ref != policy.local_route_ref
                || transfer.terminal_adapter != policy.local_terminal_adapter
            {
                return Err("custody-transfer-endpoint-mismatch".to_owned());
            }
            let authenticated = authenticate_custody_transfer(&mut authenticator, transfer)
                .map_err(|_| "custody-transfer-authentication-refused".to_owned())?;
            let receipt = ledger
                .accept_custody(
                    &authenticated,
                    &policy.local_machine_ref,
                    &policy.local_incarnation_ref,
                )
                .map_err(|_| "custody-transfer-refused".to_owned())?;
            Ok(CustodyTransportResponse::CustodyAccepted {
                schema_version: SCHEMA_VERSION,
                receipt,
            })
        }
        CustodyTransportRequest::SuccessorRebind {
            schema_version,
            rebind,
        } => {
            require_schema(schema_version)?;
            require_local_mutation_authority(policy)?;
            if rebind.new_authority_digest != policy.authority_digest
                || rebind.successor_proof_digest != peer.successor_proof_digest
                || rebind.new_target_route_ref != policy.local_route_ref
                || rebind.terminal_adapter != policy.local_terminal_adapter
            {
                return Err("custody-successor-authority-mismatch".to_owned());
            }
            let authenticated = authenticate_custody_successor_rebind(
                &mut authenticator,
                &evidence.peer_machine_ref,
                rebind,
            )
            .map_err(|_| "custody-successor-authentication-refused".to_owned())?;
            let receipt = ledger
                .accept_custody_successor_rebind(
                    &authenticated,
                    &policy.local_machine_ref,
                    &policy.local_incarnation_ref,
                    &peer.successor_proof_digest,
                )
                .map_err(|_| "custody-successor-refused".to_owned())?;
            Ok(CustodyTransportResponse::SuccessorCommitted {
                schema_version: SCHEMA_VERSION,
                receipt,
            })
        }
        CustodyTransportRequest::Control {
            schema_version,
            control,
        } => {
            require_schema(schema_version)?;
            require_local_mutation_authority(policy)?;
            if control.authority_digest != policy.authority_digest {
                return Err("custody-control-authority-mismatch".to_owned());
            }
            let authenticated = authenticate_custody_control(
                &mut authenticator,
                &evidence.peer_machine_ref,
                control,
            )
            .map_err(|_| "custody-control-authentication-refused".to_owned())?;
            let receipt = ledger
                .apply_remote_custody_control(&authenticated)
                .map_err(|_| "custody-control-refused".to_owned())?;
            Ok(CustodyTransportResponse::ControlApplied {
                schema_version: SCHEMA_VERSION,
                receipt,
            })
        }
        CustodyTransportRequest::Processed {
            schema_version,
            receipt,
        } => {
            require_schema(schema_version)?;
            if evidence.peer_machine_ref != policy.mutation_authority_machine_ref {
                return Err("custody-processed-non-authority-peer".to_owned());
            }
            let receipt_digest = receipt.receipt_digest.clone();
            let authenticated = authenticate_processed_receipt(
                &mut authenticator,
                &evidence.peer_machine_ref,
                receipt,
            )
            .map_err(|_| "custody-processed-authentication-refused".to_owned())?;
            ledger
                .acknowledge_remote_processed(&authenticated)
                .map_err(|_| "custody-processed-refused".to_owned())?;
            Ok(CustodyTransportResponse::ProcessedAcknowledged {
                schema_version: SCHEMA_VERSION,
                receipt_digest,
            })
        }
    }
}

trait CustodyCarrier {
    fn exchange(
        &mut self,
        peer: &CustodyPeer,
        request: &CustodyTransportRequest,
        deadline: Instant,
        max_output_bytes: u64,
    ) -> Result<(CustodyTransportResponse, String), String>;
}

struct SshCustodyCarrier;

impl CustodyCarrier for SshCustodyCarrier {
    fn exchange(
        &mut self,
        peer: &CustodyPeer,
        request: &CustodyTransportRequest,
        deadline: Instant,
        max_output_bytes: u64,
    ) -> Result<(CustodyTransportResponse, String), String> {
        let known_hosts = read_bounded_authority(&peer.known_hosts_file, 64 * 1024)?;
        validate_private_file(&peer.identity_file)?;
        validate_executable(&peer.ssh_program)?;
        let request =
            serde_json::to_vec(request).map_err(|_| "custody-request-encode".to_owned())?;
        let mut command = Command::new(&peer.ssh_program);
        command
            .env_clear()
            .args([
                "-F",
                "/dev/null",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=8",
                "-o",
                "StrictHostKeyChecking=yes",
                "-o",
                "CheckHostIP=yes",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "KnownHostsCommand=/usr/bin/printenv SHIPYARD_CUSTODY_KNOWN_HOSTS",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "IdentityAgent=none",
                "-o",
                "ControlMaster=no",
                "-o",
                "ProxyCommand=none",
                "-o",
                "ProxyJump=none",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ClearAllForwardings=yes",
                "-i",
            ])
            .arg(&peer.identity_file)
            .args(["-p", &peer.port.to_string(), "-s", "--", &peer.destination])
            .arg(&peer.remote_subsystem)
            .env("SHIPYARD_CUSTODY_KNOWN_HOSTS", &known_hosts)
            .env("LANG", "C")
            .env("LC_ALL", "C");
        let output = crate::process::run_output_with_input_until(
            &mut command,
            &request,
            deadline,
            "custody ssh transport",
        )
        .map_err(|_| "custody-peer-unavailable".to_owned())?;
        if !output.status.success()
            || output.stdout.len() as u64 > max_output_bytes
            || output.stderr.len() as u64 > max_output_bytes
        {
            return Err("custody-peer-refused".to_owned());
        }
        let response = serde_json::from_slice(&output.stdout)
            .map_err(|_| "custody-response-malformed".to_owned())?;
        let host_witness = hex::encode(Sha256::digest(
            format!(
                "custody-ssh-server-v1\n{}\n{}",
                peer.machine_ref,
                hex::encode(Sha256::digest(known_hosts.as_bytes()))
            )
            .as_bytes(),
        ));
        Ok((response, host_witness))
    }
}

pub(crate) struct CustodyTransportRuntime {
    policy: Option<CustodyTransportPolicy>,
    policy_refused: bool,
    state_dir: PathBuf,
    last_error: Option<String>,
    next_run_at: Instant,
    result_rx: Option<Receiver<Result<(), String>>>,
}

impl CustodyTransportRuntime {
    pub(crate) fn for_daemon(mode: RuntimeMode, global_dir: PathBuf, state_dir: PathBuf) -> Self {
        match load_custody_transport_policy(mode, global_dir) {
            Ok(policy) => Self {
                policy,
                policy_refused: false,
                state_dir,
                last_error: None,
                next_run_at: Instant::now(),
                result_rx: None,
            },
            Err(error) => Self {
                policy: None,
                policy_refused: true,
                state_dir,
                last_error: Some(error),
                next_run_at: Instant::now() + Duration::from_secs(30),
                result_rx: None,
            },
        }
    }

    pub(crate) fn diagnostic_error(&self) -> Option<String> {
        self.last_error.clone()
    }

    /// Synchronously stage local native obligations before the provider lane
    /// can inspect them. Once cross-machine policy elects another machine,
    /// this host must never race local delivery against custody transfer.
    pub(crate) fn prepare_native_obligations(&mut self) {
        let Some(policy) = self.policy.as_ref() else {
            return;
        };
        if policy.local_machine_ref == policy.mutation_authority_machine_ref {
            return;
        }
        let result = WorkLedger::open_existing(&self.state_dir)
            .map_err(|_| "custody-ledger-unavailable".to_owned())
            .and_then(|ledger| {
                ledger.map_or(Ok(()), |ledger| stage_native_obligations(policy, &ledger))
            });
        if let Err(error) = result {
            self.last_error = Some(error);
        }
    }

    /// Enabled custody policy elects exactly one machine for provider-side
    /// mutation. Omitted/default-off policy preserves the existing local lane.
    pub(crate) fn permits_local_continuation(&self) -> bool {
        !self.policy_refused
            && self.policy.as_ref().is_none_or(|policy| {
                policy.local_machine_ref == policy.mutation_authority_machine_ref
            })
    }

    pub(crate) fn tick(&mut self) {
        if let Some(receiver) = &self.result_rx {
            match receiver.try_recv() {
                Ok(result) => {
                    self.last_error = result.err();
                    self.result_rx = None;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.last_error = Some("custody-worker-disconnected".to_owned());
                    self.result_rx = None;
                }
            }
        }
        if Instant::now() < self.next_run_at {
            return;
        }
        self.next_run_at = Instant::now() + Duration::from_secs(5);
        let Some(policy) = self.policy.clone() else {
            return;
        };
        let state_dir = self.state_dir.clone();
        let (sender, receiver) = mpsc::channel();
        self.result_rx = Some(receiver);
        thread::spawn(move || {
            let _ = sender.send(reconcile_once(&policy, &state_dir, &mut SshCustodyCarrier));
        });
    }
}

#[allow(clippy::too_many_lines)] // One ordered pass preserves rebind/control/delivery/processed ordering.
fn reconcile_once<C: CustodyCarrier>(
    policy: &CustodyTransportPolicy,
    state_dir: &Path,
    carrier: &mut C,
) -> Result<(), String> {
    let Some(ledger) = WorkLedger::open_existing(state_dir)
        .map_err(|_| "custody-ledger-unavailable".to_owned())?
    else {
        return Ok(());
    };
    let mut first_error = None;
    if let Err(error) = stage_native_obligations(policy, &ledger) {
        first_error.get_or_insert(error);
    }
    for rebind in ledger
        .pending_custody_successor_rebinds(MAX_BATCH)
        .map_err(map_ledger)?
    {
        let result = (|| {
            let peer = peer(policy, &rebind.target_machine_ref)?;
            let request = CustodyTransportRequest::SuccessorRebind {
                schema_version: SCHEMA_VERSION,
                rebind,
            };
            let (response, witness) = exchange(policy, peer, &request, carrier)?;
            if let CustodyTransportResponse::SuccessorCommitted { receipt, .. } = response {
                ledger
                    .acknowledge_custody_successor_rebind(
                        &authenticate_custody_successor_receipt(
                            &mut WitnessAuthenticator::new(&peer.machine_ref, &witness),
                            &peer.machine_ref,
                            receipt,
                        )
                        .map_err(map_ledger)?,
                    )
                    .map_err(map_ledger)?;
                Ok(())
            } else {
                Err("custody-successor-response-refused".to_owned())
            }
        })();
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    for (control, machine) in ledger
        .pending_custody_controls(MAX_BATCH)
        .map_err(map_ledger)?
    {
        let result = (|| {
            let peer = peer(policy, &machine)?;
            let request = CustodyTransportRequest::Control {
                schema_version: SCHEMA_VERSION,
                control,
            };
            let (response, witness) = exchange(policy, peer, &request, carrier)?;
            if let CustodyTransportResponse::ControlApplied { receipt, .. } = response {
                ledger
                    .acknowledge_remote_custody_control(
                        &authenticate_custody_control_receipt(
                            &mut WitnessAuthenticator::new(&peer.machine_ref, &witness),
                            &peer.machine_ref,
                            receipt,
                        )
                        .map_err(map_ledger)?,
                    )
                    .map_err(map_ledger)?;
                Ok(())
            } else {
                Err("custody-control-response-refused".to_owned())
            }
        })();
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    for message_id in ledger
        .custody_send_candidates(MAX_BATCH)
        .map_err(map_ledger)?
    {
        let Ok(claim) = ledger.claim_custody_send(
            &message_id,
            &policy.sender_owner_ref,
            Utc::now() + ChronoDuration::seconds(policy.lease_seconds.cast_signed()),
        ) else {
            continue;
        };
        let result = (|| {
            let transfer = ledger.custody_transfer(&claim).map_err(map_ledger)?;
            if transfer.target_machine_ref != policy.mutation_authority_machine_ref {
                return Err("custody-target-is-not-mutation-authority".to_owned());
            }
            let peer = peer(policy, &transfer.target_machine_ref)?;
            let request = CustodyTransportRequest::Transfer {
                schema_version: SCHEMA_VERSION,
                transfer,
            };
            let (response, witness) = exchange(policy, peer, &request, carrier)?;
            if let CustodyTransportResponse::CustodyAccepted { receipt, .. } = response {
                ledger
                    .acknowledge_remote_custody(
                        &claim,
                        &authenticate_custody_receipt(
                            &mut WitnessAuthenticator::new(&peer.machine_ref, &witness),
                            &peer.machine_ref,
                            receipt,
                        )
                        .map_err(map_ledger)?,
                    )
                    .map_err(map_ledger)?;
                Ok(())
            } else {
                Err("custody-transfer-response-refused".to_owned())
            }
        })();
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    if policy.local_machine_ref == policy.mutation_authority_machine_ref {
        for message_id in ledger
            .native_custody_inbox_candidates(MAX_BATCH)
            .map_err(map_ledger)?
        {
            let result = (|| {
                let claim = ledger
                    .claim_custody_inbox(
                        &message_id,
                        &policy.inbox_owner_ref,
                        Utc::now() + ChronoDuration::seconds(policy.lease_seconds.cast_signed()),
                    )
                    .map_err(map_ledger)?;
                ledger
                    .apply_native_custody_obligation(
                        &claim,
                        &policy.local_incarnation_ref,
                        &policy.authority_digest,
                    )
                    .map_err(map_ledger)?;
                Ok(())
            })();
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
        }
    }
    for (receipt, source_machine) in ledger
        .processed_custody_receipts(MAX_BATCH)
        .map_err(map_ledger)?
    {
        let result = (|| {
            let peer = peer(policy, &source_machine)?;
            let receipt_digest = receipt.receipt_digest.clone();
            let receipt_for_ack = receipt.clone();
            let request = CustodyTransportRequest::Processed {
                schema_version: SCHEMA_VERSION,
                receipt,
            };
            let (response, _) = exchange(policy, peer, &request, carrier)?;
            match response {
                CustodyTransportResponse::ProcessedAcknowledged {
                    receipt_digest: acknowledged,
                    ..
                } if acknowledged == receipt_digest => ledger
                    .acknowledge_processed_delivery(&receipt_for_ack, &source_machine)
                    .map_err(map_ledger),
                _ => Err("custody-processed-response-refused".to_owned()),
            }
        })();
        if let Err(error) = result {
            first_error.get_or_insert(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn stage_native_obligations(
    policy: &CustodyTransportPolicy,
    ledger: &WorkLedger,
) -> Result<(), String> {
    if policy.local_machine_ref == policy.mutation_authority_machine_ref {
        return Ok(());
    }
    ledger
        .require_native_custody_cutover_ready()
        .map_err(map_ledger)?;
    let target = peer(policy, &policy.mutation_authority_machine_ref)?;
    let candidates = ledger
        .native_custody_stage_candidates(
            &policy.local_machine_ref,
            &policy.local_incarnation_ref,
            MAX_BATCH,
        )
        .map_err(map_ledger)?;
    let mut first_error = None;
    for envelope in candidates {
        if let Err(error) = ledger.stage_cross_machine_custody(
            &envelope,
            &target.machine_ref,
            &target.incarnation_ref,
            &target.route_ref,
            &target.terminal_adapter,
            &policy.authority_digest,
        ) {
            first_error.get_or_insert(map_ledger(error));
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn exchange<C: CustodyCarrier>(
    policy: &CustodyTransportPolicy,
    peer: &CustodyPeer,
    request: &CustodyTransportRequest,
    carrier: &mut C,
) -> Result<(CustodyTransportResponse, String), String> {
    carrier.exchange(
        peer,
        request,
        Instant::now() + Duration::from_secs(policy.deadline_seconds),
        policy.max_output_bytes,
    )
}

fn peer<'a>(policy: &'a CustodyTransportPolicy, machine: &str) -> Result<&'a CustodyPeer, String> {
    policy
        .peers
        .get(machine)
        .ok_or_else(|| "custody-peer-not-configured".to_owned())
}

struct BoundAuthenticator {
    peer: String,
    policy_digest: String,
    key_digest: String,
}

impl BoundAuthenticator {
    fn new(peer: &str, policy_digest: &str, key_digest: &str) -> Self {
        Self {
            peer: peer.to_owned(),
            policy_digest: policy_digest.to_owned(),
            key_digest: key_digest.to_owned(),
        }
    }
}

impl CustodyTransportAuthenticator for BoundAuthenticator {
    fn authenticate(&mut self, peer: &str, payload: &str) -> WorkLedgerResult<String> {
        if peer != self.peer {
            return Err(WorkLedgerError::Refused(
                "custody authenticated peer mismatch".to_owned(),
            ));
        }
        Ok(hex::encode(Sha256::digest(
            format!(
                "custody-incoming-v1\n{}\n{}\n{}\n{}",
                self.peer, self.policy_digest, self.key_digest, payload
            )
            .as_bytes(),
        )))
    }
}

struct WitnessAuthenticator {
    peer: String,
    witness: String,
}

impl WitnessAuthenticator {
    fn new(peer: &str, witness: &str) -> Self {
        Self {
            peer: peer.to_owned(),
            witness: witness.to_owned(),
        }
    }
}

impl CustodyTransportAuthenticator for WitnessAuthenticator {
    fn authenticate(&mut self, peer: &str, payload: &str) -> WorkLedgerResult<String> {
        if peer != self.peer {
            return Err(WorkLedgerError::Refused(
                "custody response peer mismatch".to_owned(),
            ));
        }
        Ok(hex::encode(Sha256::digest(
            format!(
                "custody-response-v1\n{}\n{}\n{}",
                self.peer, self.witness, payload
            )
            .as_bytes(),
        )))
    }
}

fn require_local_mutation_authority(policy: &CustodyTransportPolicy) -> Result<(), String> {
    if policy.local_machine_ref == policy.mutation_authority_machine_ref {
        Ok(())
    } else {
        Err("custody-local-machine-is-not-mutation-authority".to_owned())
    }
}

fn require_schema(version: u32) -> Result<(), String> {
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err("custody-schema-version-refused".to_owned())
    }
}

fn required<T>(value: Option<T>, code: &str) -> Result<T, String> {
    value.ok_or_else(|| format!("custody-policy-{code}-missing"))
}

fn absolute_path(value: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if !path.is_absolute() || path.file_name().is_none() || value.chars().any(char::is_control) {
        return Err("custody-policy-path-invalid".to_owned());
    }
    Ok(path)
}

fn safe_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.starts_with('-')
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
        })
}

fn validate_opaque(value: &str, prefix: &str) -> Result<(), String> {
    let expected = format!("{prefix}_");
    if value.len() == expected.len() + 64
        && value.starts_with(&expected)
        && value[expected.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err("custody-policy-opaque-ref-invalid".to_owned())
    }
}

fn validate_digest(value: &str) -> Result<(), String> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        Ok(())
    } else {
        Err("custody-policy-digest-invalid".to_owned())
    }
}

fn read_bounded_authority(path: &Path, max: u64) -> Result<String, String> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    let file = options
        .open(path)
        .map_err(|_| "custody-authority-unavailable".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "custody-authority-untrusted".to_owned())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max {
        return Err("custody-authority-untrusted".to_owned());
    }
    #[cfg(unix)]
    if metadata.nlink() != 1
        || (metadata.uid() != 0 && metadata.uid() != nix::unistd::Uid::effective().as_raw())
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("custody-authority-untrusted".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "custody-authority-unavailable".to_owned())?;
    if bytes.len() as u64 > max {
        return Err("custody-authority-untrusted".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "custody-authority-untrusted".to_owned())
}

fn read_exposed_ssh_key(path: &Path) -> Result<String, String> {
    let contents = read_bounded_ssh_auth_info(path, 64 * 1024)?;
    let mut keys = contents.lines().filter_map(|line| {
        let tokens = line.split_whitespace().collect::<Vec<_>>();
        let offset = usize::from(tokens.first().is_some_and(|token| *token == "publickey"));
        (tokens.len() >= offset + 2 && tokens[offset].starts_with("ssh-"))
            .then(|| format!("{} {}", tokens[offset], tokens[offset + 1]))
    });
    let key = keys
        .next()
        .ok_or_else(|| "custody-incoming-auth-info-invalid".to_owned())?;
    if keys.next().is_some() {
        return Err("custody-incoming-auth-info-ambiguous".to_owned());
    }
    Ok(key)
}

fn read_bounded_ssh_auth_info(path: &Path, max: u64) -> Result<String, String> {
    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed());
    let file = options
        .open(path)
        .map_err(|_| "custody-incoming-auth-info-missing".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "custody-incoming-auth-info-invalid".to_owned())?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > max {
        return Err("custody-incoming-auth-info-invalid".to_owned());
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err("custody-incoming-auth-info-invalid".to_owned());
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "custody-incoming-auth-info-invalid".to_owned())?;
    if bytes.len() as u64 > max {
        return Err("custody-incoming-auth-info-invalid".to_owned());
    }
    String::from_utf8(bytes).map_err(|_| "custody-incoming-auth-info-invalid".to_owned())
}

#[cfg(unix)]
fn validate_private_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|_| "custody-private-file-unavailable".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "custody-private-file-untrusted".to_owned())?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o077 != 0
    {
        return Err("custody-private-file-untrusted".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_file(_path: &Path) -> Result<(), String> {
    Err("custody-platform-unavailable".to_owned())
}

#[cfg(unix)]
fn validate_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits().cast_signed())
        .open(path)
        .map_err(|_| "custody-executable-unavailable".to_owned())?;
    let metadata = file
        .metadata()
        .map_err(|_| "custody-executable-untrusted".to_owned())?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o111 == 0
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err("custody-executable-untrusted".to_owned());
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_executable(_path: &Path) -> Result<(), String> {
    Err("custody-platform-unavailable".to_owned())
}

fn map_ledger(_error: WorkLedgerError) -> String {
    "custody-ledger-refused".to_owned()
}

#[cfg(test)]
mod tests;
