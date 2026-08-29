//! Read-only physical-readiness observation for the Pulp macOS canary.
//!
//! This module can authenticate a configured macOS host through an explicit
//! known-hosts file and collect bounded, non-mutating host facts. It cannot
//! mint session generations, authenticate a LAN route, attest capabilities or
//! cache generations, execute work, or make a canary eligible. The dry-run
//! classifier exposes those missing authorities as an `Ineligible` decision.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use crate::parallel_proof::Sha256Digest;
use crate::parallel_proof_canary::{
    CanaryIneligibleReason, PulpMacCanaryDecision, PulpMacCanaryPolicy,
};
use crate::parallel_proof_canary_cache::PulpMacCacheProbeEvidence;
use crate::process::run_output_until;

const MAX_KNOWN_HOSTS_BYTES: u64 = 64 * 1024;
const MAX_PROBE_OUTPUT_BYTES: usize = 16 * 1024;
const OBSERVER_SCHEMA: &str = "1";
const KNOWN_HOSTS_ENV: &str = "SHIPYARD_CANARY_KNOWN_HOSTS";

/// Explicit SSH authority for one read-only host observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StrictSshCanaryTarget {
    ssh_program: PathBuf,
    destination: String,
    known_hosts_file: PathBuf,
    identity_file: PathBuf,
    port: u16,
}

impl StrictSshCanaryTarget {
    /// Validate a shell-free SSH target. The known-hosts file itself is checked
    /// immediately before every observation.
    pub fn new(
        ssh_program: impl Into<PathBuf>,
        destination: impl Into<String>,
        known_hosts_file: impl Into<PathBuf>,
        identity_file: impl Into<PathBuf>,
        port: u16,
    ) -> Result<Self, CanaryObserverError> {
        let ssh_program = ssh_program.into();
        let destination = destination.into();
        let known_hosts_file = known_hosts_file.into();
        let identity_file = identity_file.into();
        if !ssh_program.is_absolute() || ssh_program.file_name().is_none() {
            return Err(CanaryObserverError::InvalidConfiguration(
                "SSH program must be an absolute executable path".to_owned(),
            ));
        }
        if destination.is_empty()
            || destination.starts_with('-')
            || destination.len() > 512
            || !destination.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
            })
        {
            return Err(CanaryObserverError::InvalidConfiguration(
                "SSH destination is not a canonical host token".to_owned(),
            ));
        }
        if !known_hosts_file.is_absolute() || known_hosts_file.file_name().is_none() {
            return Err(CanaryObserverError::InvalidConfiguration(
                "known-hosts authority must be an absolute file path".to_owned(),
            ));
        }
        if known_hosts_file
            .to_str()
            .is_none_or(|path| path.chars().any(char::is_control))
        {
            return Err(CanaryObserverError::InvalidConfiguration(
                "known-hosts authority path must be valid control-free UTF-8".to_owned(),
            ));
        }
        if !identity_file.is_absolute()
            || identity_file.file_name().is_none()
            || identity_file
                .to_str()
                .is_none_or(|path| path.chars().any(char::is_control))
        {
            return Err(CanaryObserverError::InvalidConfiguration(
                "SSH identity authority must be an absolute control-free file path".to_owned(),
            ));
        }
        if port == 0 {
            return Err(CanaryObserverError::InvalidConfiguration(
                "SSH port must be nonzero".to_owned(),
            ));
        }
        Ok(Self {
            ssh_program,
            destination,
            known_hosts_file,
            identity_file,
            port,
        })
    }

    /// Prepare one shell-free, ambient-config-free remote command while
    /// retaining the exact known-host authority for post-execution recheck.
    pub(crate) fn prepare_remote_command(
        &self,
        remote_program: &str,
        remote_arguments: &[&str],
    ) -> Result<StrictSshCommand, CanaryObserverError> {
        if !safe_remote_token(remote_program)
            || !remote_program.starts_with('/')
            || remote_arguments
                .iter()
                .any(|value| !safe_remote_token(value))
        {
            return Err(CanaryObserverError::InvalidConfiguration(
                "remote command tokens must be absolute or bounded shell-free values".to_owned(),
            ));
        }
        validate_executable(&self.ssh_program)?;
        validate_private_identity_authority(&self.identity_file)?;
        let authority = KnownHostsAuthority::open(&self.known_hosts_file)?;
        let known_hosts_sha256 = authority.digest.clone();
        let mut command = Command::new(&self.ssh_program);
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
                "KnownHostsCommand=/usr/bin/printenv SHIPYARD_CANARY_KNOWN_HOSTS",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "IdentityAgent=none",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
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
            .arg(&self.identity_file)
            .args(["-p", &self.port.to_string(), "-o"])
            .args([
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "UpdateHostKeys=no",
                "--",
                &self.destination,
                remote_program,
            ])
            .args(remote_arguments)
            .env(KNOWN_HOSTS_ENV, authority.contents())
            .env("LANG", "C")
            .env("LC_ALL", "C");
        Ok(StrictSshCommand {
            command,
            authority,
            destination: self.destination.clone(),
            known_hosts_sha256,
        })
    }
}

/// One exact strict-SSH command plus its held host-key authority.
pub(crate) struct StrictSshCommand {
    pub(crate) command: Command,
    authority: KnownHostsAuthority,
    pub(crate) destination: String,
    pub(crate) known_hosts_sha256: Sha256Digest,
}

impl StrictSshCommand {
    /// Re-read the locked host-key authority after the SSH process completes.
    pub(crate) fn verify_unchanged(&mut self) -> Result<(), CanaryObserverError> {
        self.authority.verify_unchanged()
    }
}

/// Transport used to collect one host's read-only facts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReadOnlyCanaryTarget {
    /// Observe the controller host directly.
    Local,
    /// Observe a remote host using strict pre-existing known-host authority.
    StrictSsh(StrictSshCanaryTarget),
}

/// Complete configuration for one bounded read-only macOS observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadOnlyCanaryHostSpec {
    host_id: String,
    target: ReadOnlyCanaryTarget,
    expected_platform_identity_sha256: Sha256Digest,
    staging_root: PathBuf,
}

impl ReadOnlyCanaryHostSpec {
    /// Construct one host specification without probing or creating anything.
    pub fn new(
        host_id: impl Into<String>,
        target: ReadOnlyCanaryTarget,
        expected_platform_identity_sha256: Sha256Digest,
        staging_root: impl Into<PathBuf>,
    ) -> Result<Self, CanaryObserverError> {
        let host_id = host_id.into();
        let staging_root = staging_root.into();
        if !valid_label(&host_id) {
            return Err(CanaryObserverError::InvalidConfiguration(
                "host id is not a canonical label".to_owned(),
            ));
        }
        if !safe_absolute_macos_path(&staging_root) {
            return Err(CanaryObserverError::InvalidConfiguration(
                "staging root must be a canonical persistent macOS path".to_owned(),
            ));
        }
        Ok(Self {
            host_id,
            target,
            expected_platform_identity_sha256,
            staging_root,
        })
    }
}

/// Authenticated transport boundary retained with a read-only observation.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CanaryObservationTransport {
    /// Direct observation of the controller host.
    Local,
    /// Strict SSH observation tied to exact known-host file contents.
    StrictSsh {
        /// Canonical configured SSH destination.
        destination: String,
        /// Digest of the bounded known-host authority read for this probe.
        known_hosts_sha256: Sha256Digest,
    },
}

/// Bounded facts collected without creating a staging root or opening caches.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadOnlyCanaryHostReceipt {
    host_id: String,
    observed_at_ms: u64,
    probe_elapsed_ms: u64,
    platform_identity_sha256: Sha256Digest,
    boot_session_sha256: Sha256Digest,
    transport: CanaryObservationTransport,
    configured_staging_root: String,
    observed_staging_root: Option<String>,
    free_bytes: Option<u64>,
    model_calls: u64,
}

impl ReadOnlyCanaryHostReceipt {
    /// Stable configured fleet host identifier.
    #[must_use]
    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// Controller epoch time at which this observation completed.
    #[must_use]
    pub fn observed_at_ms(&self) -> u64 {
        self.observed_at_ms
    }

    /// Transport authority used for this observation.
    #[must_use]
    pub fn transport(&self) -> &CanaryObservationTransport {
        &self.transport
    }

    /// Canonical existing staging root, or `None` when it was absent.
    #[must_use]
    pub fn observed_staging_root(&self) -> Option<&str> {
        self.observed_staging_root.as_deref()
    }

    /// Exact free bytes observed for the existing staging filesystem.
    #[must_use]
    pub fn free_bytes(&self) -> Option<u64> {
        self.free_bytes
    }

    /// Monotonic elapsed time for the bounded observation.
    #[must_use]
    pub fn probe_elapsed_ms(&self) -> u64 {
        self.probe_elapsed_ms
    }

    /// Model calls used by routine readiness observation; always zero.
    #[must_use]
    pub const fn model_calls(&self) -> u64 {
        self.model_calls
    }

    /// Domain-separated digest of the complete immutable observation.
    pub fn digest(&self) -> Result<Sha256Digest, CanaryObserverError> {
        let bytes = serde_json::to_vec(self)
            .map_err(|error| CanaryObserverError::MalformedOutput(error.to_string()))?;
        let mut bound = b"shipyard.pulp-mac-canary.read-only-host.v1\0".to_vec();
        bound.extend_from_slice(&bytes);
        Ok(Sha256Digest::of_bytes(&bound))
    }
}

/// Stable missing authorities surfaced by the dry-run controller.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PhysicalCanaryReadinessGap {
    /// Exactly one current observation for the configured host was unavailable.
    HostObservationMissing {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// The observation is expired, future-dated, or lacks a usable policy clock.
    HostObservationStale {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// No durable authority mapped the boot identity to a nonzero generation.
    SessionGenerationAuthorityMissing {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// No authenticated direct-LAN route receipt exists for the worker.
    LanRouteAuthorityMissing {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// The declared persistent staging root does not currently exist.
    StagingRootUnavailable {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// Current free bytes cannot prove the artifact-plus-reserve fence.
    StorageReserveUnproven {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// No immutable cache-generation authority was observed.
    CacheGenerationAuthorityMissing {
        /// Stable configured host identifier.
        host_id: String,
    },
    /// Runtime capabilities have no authenticated observation authority.
    CapabilityAuthorityMissing {
        /// Stable configured host identifier.
        host_id: String,
    },
}

/// Non-mutating physical readiness result. This tranche cannot return Eligible.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacDryRunReadiness {
    decision: PulpMacCanaryDecision,
    gaps: Vec<PhysicalCanaryReadinessGap>,
    model_calls: u64,
    would_mutate: bool,
}

impl PulpMacDryRunReadiness {
    /// Fail-closed canary decision derived from the current missing proofs.
    #[must_use]
    pub fn decision(&self) -> &PulpMacCanaryDecision {
        &self.decision
    }

    /// Sorted, deduplicated physical proof gaps.
    #[must_use]
    pub fn gaps(&self) -> &[PhysicalCanaryReadinessGap] {
        &self.gaps
    }

    /// Model calls used by classification; always zero.
    #[must_use]
    pub const fn model_calls(&self) -> u64 {
        self.model_calls
    }

    /// Whether this readiness operation would mutate external state; always false.
    #[must_use]
    pub const fn would_mutate(&self) -> bool {
        self.would_mutate
    }
}

/// Bounded command result supplied by the observer's process boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReadOnlyProbeOutput {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl ReadOnlyProbeOutput {
    /// Test/adapter constructor. Implementations of the runner trait are the
    /// process-authentication boundary and must not copy host claims blindly.
    #[must_use]
    pub fn new(success: bool, stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        Self {
            success,
            stdout,
            stderr,
        }
    }
}

/// Injectable process edge for strict host observations.
pub trait ReadOnlyCanaryProbeRunner {
    /// Execute one observer-assembled command under its absolute deadline.
    fn run(
        &mut self,
        command: &mut Command,
        deadline: Instant,
        label: &str,
    ) -> Result<ReadOnlyProbeOutput, CanaryObserverError>;
}

/// Production bounded process runner. It only executes commands assembled by
/// [`StrictKnownHostCanaryObserver`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemReadOnlyCanaryProbeRunner;

impl ReadOnlyCanaryProbeRunner for SystemReadOnlyCanaryProbeRunner {
    fn run(
        &mut self,
        command: &mut Command,
        deadline: Instant,
        label: &str,
    ) -> Result<ReadOnlyProbeOutput, CanaryObserverError> {
        let output = run_output_until(command, deadline, label)
            .map_err(|error| CanaryObserverError::Command(error.to_string()))?;
        Ok(ReadOnlyProbeOutput::new(
            output.status.success(),
            output.stdout,
            output.stderr,
        ))
    }
}

/// Strict, bounded, read-only macOS observer.
pub struct StrictKnownHostCanaryObserver<R = SystemReadOnlyCanaryProbeRunner> {
    runner: R,
}

impl Default for StrictKnownHostCanaryObserver<SystemReadOnlyCanaryProbeRunner> {
    fn default() -> Self {
        Self {
            runner: SystemReadOnlyCanaryProbeRunner,
        }
    }
}

impl<R: ReadOnlyCanaryProbeRunner> StrictKnownHostCanaryObserver<R> {
    /// Inject a process boundary, primarily for deterministic contract tests.
    #[must_use]
    pub fn with_runner(runner: R) -> Self {
        Self { runner }
    }

    /// Collect one exact host receipt. No missing proof is inferred or filled.
    pub fn observe(
        &mut self,
        spec: &ReadOnlyCanaryHostSpec,
        timeout: Duration,
    ) -> Result<ReadOnlyCanaryHostReceipt, CanaryObserverError> {
        if timeout.is_zero() || timeout > Duration::from_secs(30) {
            return Err(CanaryObserverError::InvalidConfiguration(
                "host observation timeout must be within 1ns..=30s".to_owned(),
            ));
        }
        let mut observation = observation_command(spec)?;
        let started = Instant::now();
        let output = self.runner.run(
            &mut observation.command,
            started + timeout,
            &format!(
                "Pulp macOS canary read-only observation for {}",
                spec.host_id
            ),
        )?;
        let elapsed = milliseconds_ceil(started.elapsed())?;
        if let Some(authority) = observation.known_hosts.as_mut() {
            authority.verify_unchanged()?;
        }
        if !output.success {
            return Err(CanaryObserverError::Command(format!(
                "host observation failed: {}",
                bounded_stderr(&output.stderr)
            )));
        }
        if output.stdout.len() > MAX_PROBE_OUTPUT_BYTES {
            return Err(CanaryObserverError::MalformedOutput(
                "host observation exceeded the fixed output limit".to_owned(),
            ));
        }
        let parsed = parse_probe_output(&output.stdout)?;
        let platform_identity_sha256 = Sha256Digest::of_bytes(parsed.platform_uuid.as_bytes());
        if platform_identity_sha256 != spec.expected_platform_identity_sha256 {
            return Err(CanaryObserverError::IdentityMismatch);
        }
        let boot_session_sha256 = Sha256Digest::of_bytes(
            format!("{}\n{}", parsed.platform_uuid, parsed.boot_seconds).as_bytes(),
        );
        let configured_staging_root = spec.staging_root.to_string_lossy().into_owned();
        let (observed_staging_root, free_bytes) = match parsed.staging {
            ParsedStaging::Missing => (None, None),
            ParsedStaging::Present {
                canonical,
                free_bytes,
            } => {
                if canonical != configured_staging_root {
                    return Err(CanaryObserverError::MalformedOutput(
                        "staging root canonical identity drifted".to_owned(),
                    ));
                }
                (Some(canonical), Some(free_bytes))
            }
        };
        Ok(ReadOnlyCanaryHostReceipt {
            host_id: spec.host_id.clone(),
            observed_at_ms: controller_now_ms()?,
            probe_elapsed_ms: elapsed,
            platform_identity_sha256,
            boot_session_sha256,
            transport: observation.transport,
            configured_staging_root,
            observed_staging_root,
            free_bytes,
            model_calls: 0,
        })
    }
}

/// Classify current physical readiness without permitting execution. Session,
/// route, capability, and cache authorities deliberately remain missing in this
/// tranche, so the returned canary decision is always `Ineligible`.
pub fn classify_pulp_mac_dry_run_readiness(
    policy: &PulpMacCanaryPolicy,
    artifact_bytes_total: u64,
    receipts: &[ReadOnlyCanaryHostReceipt],
) -> Result<PulpMacDryRunReadiness, CanaryObserverError> {
    classify_pulp_mac_dry_run_readiness_with_cache(policy, artifact_bytes_total, receipts, None)
}

/// Classify dry-run physical readiness with optional exact paired cache proof.
///
/// Valid cache evidence closes only the cache-generation gap. Session, route,
/// capability, staging, and reserve authorities remain independently required,
/// so this library tranche still cannot return `Eligible`.
pub fn classify_pulp_mac_dry_run_readiness_with_cache(
    policy: &PulpMacCanaryPolicy,
    artifact_bytes_total: u64,
    receipts: &[ReadOnlyCanaryHostReceipt],
    cache_evidence: Option<&PulpMacCacheProbeEvidence>,
) -> Result<PulpMacDryRunReadiness, CanaryObserverError> {
    if artifact_bytes_total == 0 {
        return Err(CanaryObserverError::InvalidConfiguration(
            "artifact byte size must be nonzero".to_owned(),
        ));
    }
    let mut gaps = BTreeSet::new();
    let cache_proof = cache_evidence_proves_current_hosts(
        policy,
        artifact_bytes_total,
        receipts,
        cache_evidence,
    )?;
    let mut reasons = BTreeSet::from([
        CanaryIneligibleReason::SessionGenerationMissing,
        CanaryIneligibleReason::CapabilityMismatch,
    ]);
    if !cache_proof.remote_worker {
        reasons.insert(CanaryIneligibleReason::RouteIneligible);
    }
    if !cache_proof.cache_generations {
        reasons.insert(CanaryIneligibleReason::CacheGenerationMismatch);
    }
    for host_id in [&policy.builder_host_id, &policy.worker_host_id] {
        let mut matching = receipts
            .iter()
            .filter(|receipt| &receipt.host_id == host_id);
        let receipt = matching.next();
        if receipt.is_none() || matching.next().is_some() {
            gaps.insert(PhysicalCanaryReadinessGap::HostObservationMissing {
                host_id: host_id.clone(),
            });
            reasons.insert(CanaryIneligibleReason::HostMissing);
            continue;
        }
        let receipt = receipt.expect("checked above");
        let remote_worker_proven = host_id == &policy.worker_host_id && cache_proof.remote_worker;
        if !remote_worker_proven {
            gaps.insert(
                PhysicalCanaryReadinessGap::SessionGenerationAuthorityMissing {
                    host_id: host_id.clone(),
                },
            );
        }
        if !cache_proof.cache_generations {
            gaps.insert(
                PhysicalCanaryReadinessGap::CacheGenerationAuthorityMissing {
                    host_id: host_id.clone(),
                },
            );
        }
        // This dry-run surface has no selected proof inventory, so even a
        // transport-authenticated capability set cannot prove the workload's
        // per-test requirements.
        gaps.insert(PhysicalCanaryReadinessGap::CapabilityAuthorityMissing {
            host_id: host_id.clone(),
        });
        if host_id == &policy.worker_host_id && !remote_worker_proven {
            gaps.insert(PhysicalCanaryReadinessGap::LanRouteAuthorityMissing {
                host_id: host_id.clone(),
            });
        }
        let stale = policy.assessed_at_ms == 0
            || policy.maximum_observation_age_ms == 0
            || receipt.observed_at_ms > policy.assessed_at_ms
            || policy.assessed_at_ms.saturating_sub(receipt.observed_at_ms)
                > policy.maximum_observation_age_ms;
        if stale {
            gaps.insert(PhysicalCanaryReadinessGap::HostObservationStale {
                host_id: host_id.clone(),
            });
            gaps.insert(PhysicalCanaryReadinessGap::StorageReserveUnproven {
                host_id: host_id.clone(),
            });
            reasons.insert(CanaryIneligibleReason::StaleObservation);
            reasons.insert(CanaryIneligibleReason::InsufficientSpace);
            continue;
        }
        if receipt.observed_staging_root.is_none() {
            gaps.insert(PhysicalCanaryReadinessGap::StagingRootUnavailable {
                host_id: host_id.clone(),
            });
            reasons.insert(CanaryIneligibleReason::StagingRootInvalid);
        }
        let required = policy.minimum_free_bytes.checked_add(artifact_bytes_total);
        if required.is_none_or(|required| receipt.free_bytes.is_none_or(|free| free < required)) {
            gaps.insert(PhysicalCanaryReadinessGap::StorageReserveUnproven {
                host_id: host_id.clone(),
            });
            reasons.insert(CanaryIneligibleReason::InsufficientSpace);
        }
    }
    Ok(PulpMacDryRunReadiness {
        decision: PulpMacCanaryDecision::Ineligible {
            reasons: reasons.into_iter().collect(),
        },
        gaps: gaps.into_iter().collect(),
        model_calls: 0,
        would_mutate: false,
    })
}

fn cache_evidence_proves_current_hosts(
    policy: &PulpMacCanaryPolicy,
    artifact_bytes_total: u64,
    receipts: &[ReadOnlyCanaryHostReceipt],
    cache_evidence: Option<&PulpMacCacheProbeEvidence>,
) -> Result<CacheReadinessProof, CanaryObserverError> {
    let unique_host_receipt = |host_id: &str| {
        let mut matching = receipts
            .iter()
            .filter(|receipt| receipt.host_id() == host_id);
        let receipt = matching.next();
        receipt.filter(|_| matching.next().is_none())
    };
    match (
        cache_evidence,
        unique_host_receipt(&policy.builder_host_id),
        unique_host_receipt(&policy.worker_host_id),
    ) {
        (Some(evidence), Some(builder), Some(worker)) => {
            let builder_digest = builder.digest()?;
            let worker_digest = worker.digest()?;
            Ok(CacheReadinessProof {
                cache_generations: evidence.proves_policy_and_hosts(
                    policy,
                    &builder_digest,
                    &worker_digest,
                ),
                remote_worker: evidence
                    .remote_worker_authority(policy, &worker_digest, artifact_bytes_total)
                    .is_some(),
            })
        }
        _ => Ok(CacheReadinessProof::default()),
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CacheReadinessProof {
    cache_generations: bool,
    remote_worker: bool,
}

/// Failure at the read-only observation boundary.
#[derive(Debug)]
pub enum CanaryObserverError {
    /// Caller supplied unsafe or unsupported configuration.
    InvalidConfiguration(String),
    /// The explicit known-host authority was absent, unsafe, or changed.
    AuthorityUnreadable(String),
    /// The bounded read-only command could not produce successful evidence.
    Command(String),
    /// Successful command output did not match the strict observer schema.
    MalformedOutput(String),
    /// The observed platform UUID did not match machine-global authority.
    IdentityMismatch,
    /// Controller epoch or monotonic timing could not be represented safely.
    Clock(String),
}

impl std::fmt::Display for CanaryObserverError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfiguration(message) => write!(
                formatter,
                "invalid canary observer configuration: {message}"
            ),
            Self::AuthorityUnreadable(message) => {
                write!(formatter, "canary observer authority unreadable: {message}")
            }
            Self::Command(message) => {
                write!(formatter, "canary observer command failed: {message}")
            }
            Self::MalformedOutput(message) => {
                write!(formatter, "malformed canary observer output: {message}")
            }
            Self::IdentityMismatch => formatter.write_str("canary observer host identity mismatch"),
            Self::Clock(message) => write!(formatter, "canary observer clock failed: {message}"),
        }
    }
}

impl std::error::Error for CanaryObserverError {}

struct ObservationCommand {
    command: Command,
    transport: CanaryObservationTransport,
    known_hosts: Option<KnownHostsAuthority>,
}

fn observation_command(
    spec: &ReadOnlyCanaryHostSpec,
) -> Result<ObservationCommand, CanaryObserverError> {
    let script = macos_observer_script(&spec.staging_root);
    match &spec.target {
        ReadOnlyCanaryTarget::Local => {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &script]);
            Ok(ObservationCommand {
                command,
                transport: CanaryObservationTransport::Local,
                known_hosts: None,
            })
        }
        ReadOnlyCanaryTarget::StrictSsh(target) => {
            validate_executable(&target.ssh_program)?;
            validate_regular_authority(&target.identity_file, "SSH identity")?;
            let authority = KnownHostsAuthority::open(&target.known_hosts_file)?;
            let known_hosts_sha256 = authority.digest.clone();
            let mut command = Command::new(&target.ssh_program);
            command.args([
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
                "KnownHostsCommand=/usr/bin/printenv SHIPYARD_CANARY_KNOWN_HOSTS",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "ControlMaster=no",
                "-o",
                "ControlPath=none",
                "-o",
                "ProxyCommand=none",
                "-o",
                "ProxyJump=none",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "ClearAllForwardings=yes",
                "-i",
            ]);
            command.arg(&target.identity_file);
            command.args(["-p", &target.port.to_string(), "-o"]);
            command.args([
                "GlobalKnownHostsFile=/dev/null",
                "-o",
                "UpdateHostKeys=no",
                "--",
                &target.destination,
                &script,
            ]);
            command.env(KNOWN_HOSTS_ENV, authority.contents());
            Ok(ObservationCommand {
                command,
                transport: CanaryObservationTransport::StrictSsh {
                    destination: target.destination.clone(),
                    known_hosts_sha256,
                },
                known_hosts: Some(authority),
            })
        }
    }
}

fn validate_executable(path: &Path) -> Result<(), CanaryObserverError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{} is not a regular non-symlink executable",
            path.display()
        )));
    }
    Ok(())
}

fn validate_regular_authority(path: &Path, label: &str) -> Result<(), CanaryObserverError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{label} {} is not a regular non-symlink file",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_identity_authority(path: &Path) -> Result<(), CanaryObserverError> {
    use std::os::unix::fs::PermissionsExt;

    validate_regular_authority(path, "SSH identity")?;
    let metadata = fs::metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.uid() != nix::unistd::Uid::effective().as_raw()
        || metadata.permissions().mode() & 0o777 != 0o600
    {
        return Err(CanaryObserverError::AuthorityUnreadable(
            "SSH identity must be current-user-owned mode 0600".to_owned(),
        ));
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_private_identity_authority(_path: &Path) -> Result<(), CanaryObserverError> {
    Err(CanaryObserverError::InvalidConfiguration(
        "strict SSH identity authority requires a Unix controller".to_owned(),
    ))
}

fn safe_remote_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
}

#[cfg(unix)]
struct KnownHostsAuthority {
    file: File,
    contents: String,
    digest: Sha256Digest,
}

#[cfg(unix)]
impl KnownHostsAuthority {
    fn open(path: &Path) -> Result<Self, CanaryObserverError> {
        let before = known_hosts_path_metadata(path)?;
        let mut file = File::open(path).map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
        })?;
        file.lock_shared().map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!(
                "{} shared-lock failed: {error}",
                path.display()
            ))
        })?;
        let opened = file.metadata().map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
        })?;
        let after = known_hosts_path_metadata(path)?;
        if !same_file_identity(&before, &opened) || !same_file_identity(&opened, &after) {
            return Err(CanaryObserverError::AuthorityUnreadable(format!(
                "{} changed while being opened",
                path.display()
            )));
        }
        let (digest, bytes) = read_known_hosts(&mut file, opened.len(), path)?;
        let contents = String::from_utf8(bytes).map_err(|_| {
            CanaryObserverError::AuthorityUnreadable(format!("{} is not UTF-8", path.display()))
        })?;
        Ok(Self {
            file,
            contents,
            digest,
        })
    }

    fn contents(&self) -> &str {
        &self.contents
    }

    fn verify_unchanged(&mut self) -> Result<(), CanaryObserverError> {
        let length = self
            .file
            .metadata()
            .map_err(|error| CanaryObserverError::AuthorityUnreadable(error.to_string()))?
            .len();
        let (observed, _) =
            read_known_hosts(&mut self.file, length, Path::new("known-host authority"))?;
        if observed != self.digest {
            return Err(CanaryObserverError::AuthorityUnreadable(
                "known-host authority changed during observation".to_owned(),
            ));
        }
        Ok(())
    }
}

#[cfg(not(unix))]
struct KnownHostsAuthority;

#[cfg(not(unix))]
impl KnownHostsAuthority {
    fn open(_path: &Path) -> Result<Self, CanaryObserverError> {
        Err(CanaryObserverError::InvalidConfiguration(
            "strict macOS observer requires a Unix controller".to_owned(),
        ))
    }

    fn contents(&self) -> &str {
        unreachable!("unsupported controller")
    }

    fn verify_unchanged(&mut self) -> Result<(), CanaryObserverError> {
        unreachable!("unsupported controller")
    }
}

#[cfg(unix)]
fn known_hosts_path_metadata(path: &Path) -> Result<fs::Metadata, CanaryObserverError> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_KNOWN_HOSTS_BYTES
    {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{} is not a bounded regular non-symlink file",
            path.display()
        )));
    }
    Ok(metadata)
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.len() == right.len()
}

#[cfg(unix)]
fn read_known_hosts(
    file: &mut File,
    expected_length: u64,
    path: &Path,
) -> Result<(Sha256Digest, Vec<u8>), CanaryObserverError> {
    let capacity = usize::try_from(expected_length).map_err(|_| {
        CanaryObserverError::AuthorityUnreadable(format!(
            "{} is too large for this controller",
            path.display()
        ))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take(MAX_KNOWN_HOSTS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
        })?;
    if bytes.len() as u64 != expected_length {
        return Err(CanaryObserverError::AuthorityUnreadable(format!(
            "{} changed while being read",
            path.display()
        )));
    }
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        CanaryObserverError::AuthorityUnreadable(format!("{}: {error}", path.display()))
    })?;
    Ok((Sha256Digest::of_bytes(&bytes), bytes))
}

fn macos_observer_script(staging_root: &Path) -> String {
    let root = shell_quote(&staging_root.to_string_lossy());
    format!(
        "set -eu\nroot={root}\nplatform_uuid=$(/usr/sbin/ioreg -rd1 -c IOPlatformExpertDevice | /usr/bin/awk -F '\"' '/IOPlatformUUID/ {{print $(NF-1); exit}}')\nboot_seconds=$(/usr/sbin/sysctl -n kern.boottime | /usr/bin/awk -F '[=,]' '{{gsub(/ /, \"\", $2); print $2}}')\nprintf 'schema\\t1\\nplatform_uuid\\t%s\\nboot_seconds\\t%s\\n' \"$platform_uuid\" \"$boot_seconds\"\nif test -d \"$root\" && test ! -L \"$root\"; then\n  identity_before=$(/usr/bin/stat -f '%d:%i' \"$root\")\n  canonical=$(cd \"$root\" && /bin/pwd -P)\n  identity_canonical=$(/usr/bin/stat -f '%d:%i' \"$canonical\")\n  free_kib=$(/bin/df -Pk \"$canonical\" | /usr/bin/awk 'END {{print $4}}')\n  identity_after=$(/usr/bin/stat -f '%d:%i' \"$root\")\n  test ! -L \"$root\"\n  test \"$identity_before\" = \"$identity_canonical\"\n  test \"$identity_before\" = \"$identity_after\"\n  printf 'staging\\tpresent\\ncanonical_root\\t%s\\nfree_kib\\t%s\\n' \"$canonical\" \"$free_kib\"\nelse\n  printf 'staging\\tmissing\\n'\nfi"
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

struct ParsedProbe {
    platform_uuid: String,
    boot_seconds: u64,
    staging: ParsedStaging,
}

enum ParsedStaging {
    Missing,
    Present { canonical: String, free_bytes: u64 },
}

fn parse_probe_output(bytes: &[u8]) -> Result<ParsedProbe, CanaryObserverError> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CanaryObserverError::MalformedOutput("output is not UTF-8".to_owned()))?;
    let mut fields = std::collections::BTreeMap::new();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('\t') else {
            return Err(CanaryObserverError::MalformedOutput(
                "output contains a non-field line".to_owned(),
            ));
        };
        if fields.insert(key, value).is_some() {
            return Err(CanaryObserverError::MalformedOutput(format!(
                "duplicate {key} field"
            )));
        }
    }
    if fields.remove("schema") != Some(OBSERVER_SCHEMA) {
        return Err(CanaryObserverError::MalformedOutput(
            "unsupported observer schema".to_owned(),
        ));
    }
    let platform_uuid = fields
        .remove("platform_uuid")
        .filter(|value| valid_platform_uuid(value))
        .ok_or_else(|| CanaryObserverError::MalformedOutput("invalid platform UUID".to_owned()))?
        .to_owned();
    let boot_seconds = fields
        .remove("boot_seconds")
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value != 0)
        .ok_or_else(|| CanaryObserverError::MalformedOutput("invalid boot session".to_owned()))?;
    let staging = match fields.remove("staging") {
        Some("missing") if fields.is_empty() => ParsedStaging::Missing,
        Some("present") => {
            let canonical = fields
                .remove("canonical_root")
                .filter(|value| safe_absolute_macos_path(Path::new(value)))
                .ok_or_else(|| {
                    CanaryObserverError::MalformedOutput(
                        "invalid canonical staging root".to_owned(),
                    )
                })?
                .to_owned();
            let free_kib = fields
                .remove("free_kib")
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| {
                    CanaryObserverError::MalformedOutput(
                        "invalid staging free-space value".to_owned(),
                    )
                })?;
            if !fields.is_empty() {
                return Err(CanaryObserverError::MalformedOutput(
                    "unknown observer fields".to_owned(),
                ));
            }
            ParsedStaging::Present {
                canonical,
                free_bytes: free_kib.checked_mul(1024).ok_or_else(|| {
                    CanaryObserverError::MalformedOutput(
                        "staging free-space value overflows bytes".to_owned(),
                    )
                })?,
            }
        }
        _ => {
            return Err(CanaryObserverError::MalformedOutput(
                "invalid staging observation".to_owned(),
            ));
        }
    };
    Ok(ParsedProbe {
        platform_uuid,
        boot_seconds,
        staging,
    })
}

fn valid_platform_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn safe_absolute_macos_path(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    path.is_absolute()
        && value != "/"
        && !value.ends_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
        && [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
        ]
        .iter()
        .all(|temporary| value != *temporary && !value.starts_with(&format!("{temporary}/")))
}

fn milliseconds_ceil(duration: Duration) -> Result<u64, CanaryObserverError> {
    let millis = duration.as_millis();
    let millis = if duration.subsec_nanos().is_multiple_of(1_000_000) {
        millis
    } else {
        millis
            .checked_add(1)
            .ok_or_else(|| CanaryObserverError::Clock("monotonic duration overflow".to_owned()))?
    };
    u64::try_from(millis)
        .map_err(|_| CanaryObserverError::Clock("monotonic duration overflow".to_owned()))
}

fn controller_now_ms() -> Result<u64, CanaryObserverError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CanaryObserverError::Clock(error.to_string()))?
        .as_millis();
    u64::try_from(millis)
        .map_err(|_| CanaryObserverError::Clock("controller epoch overflow".to_owned()))
}

fn bounded_stderr(bytes: &[u8]) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(512)])
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests;
