//! Authenticated companion protocol for read-only remote cache observation.
//!
//! Historical `M1` type names remain for API stability; live builder and
//! worker host identities are explicit constructor inputs and receipt data.
//!
//! The production carrier tries an explicit pinned direct-LAN target first and
//! may use an independently pinned Tailscale target only after a classified
//! transport failure. Both routes reject ambient SSH configuration and bind
//! the verified host session, capabilities, staging reserve, terminal instance,
//! and companion executable to one invocation.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::parallel_proof::{MAX_CAPABILITIES, Sha256Digest};
use crate::parallel_proof_canary::{CanaryRoute, CanaryStagingClass};
use crate::parallel_proof_canary_cache::{
    CacheGenerationManifest, CacheGenerationObservationReceipt, CacheGenerationObserver,
    CacheGenerationProbeSpec, CacheObserverError, LocalCacheGenerationObserver,
    produce_cache_generation_manifest,
};
use crate::parallel_proof_canary_controller::StrictSshCanaryTarget;
use crate::process::ProcessTree;
use crate::workstream_provider_adapter::verify_current_companion_digest;

/// Current strict remote-cache companion protocol.
pub const REMOTE_M1_CACHE_PROTOCOL_SCHEMA: u32 = 1;
const MAX_COMPANION_MESSAGE_BYTES: u64 = 20 * 1024 * 1024;
const MAX_COMPANION_STDERR_BYTES: u64 = 64 * 1024;

/// Authenticated facts established before invoking the remote companion.
///
/// Construction validates shape only. Implementations of
/// [`RemoteM1CacheTransport`] own the trust boundary and must populate these
/// fields from protected controller/terminal receipts, never worker claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteM1CacheAuthority {
    /// Stable controller host initiating the observation.
    pub source_host_id: String,
    /// Stable configured worker host.
    pub host_id: String,
    /// Digest of the exact read-only host receipt used by the controller.
    pub host_observation_sha256: Sha256Digest,
    /// Nonzero reconnect-fenced host session generation.
    pub host_session_generation: u64,
    /// Exact pinned route used from builder to worker; Tailnet is diagnostic-only.
    pub route: CanaryRoute,
    /// Exact SSH destination selected for this route.
    pub destination: String,
    /// Digest of the locked known-host authority used by SSH.
    pub known_hosts_sha256: Sha256Digest,
    /// Sorted unique authenticated execution capabilities.
    pub capabilities: Vec<String>,
    /// Persistent configured staging root.
    pub staging_root: String,
    /// Authenticated storage classification.
    pub staging_class: CanaryStagingClass,
    /// Current free bytes on the staging filesystem.
    pub free_bytes: u64,
    /// Artifact bytes that would need staging in a later physical canary.
    pub artifact_bytes_total: u64,
    /// Bytes that must remain free after staging.
    pub minimum_reserve_bytes: u64,
    /// Digest of the adapter-verified terminal instance receipt.
    pub terminal_instance_sha256: Sha256Digest,
    /// Digest of the exact installed companion executable.
    pub companion_executable_sha256: Sha256Digest,
    /// Controller time of this authenticated authority observation.
    pub observed_at_ms: u64,
    /// Routine observation uses no model.
    pub model_calls: u64,
}

impl RemoteM1CacheAuthority {
    /// Validate the complete remote authority without invoking the companion.
    pub fn validate(&self) -> Result<(), CacheObserverError> {
        let required_free = self
            .artifact_bytes_total
            .checked_add(self.minimum_reserve_bytes);
        if !valid_host_id(&self.source_host_id)
            || !valid_host_id(&self.host_id)
            || self.source_host_id == self.host_id
            || self.host_session_generation == 0
            || !matches!(self.route, CanaryRoute::Lan | CanaryRoute::Tailnet)
            || !safe_destination(&self.destination)
            || self.capabilities.is_empty()
            || self.capabilities.len() > MAX_CAPABILITIES
            || !strictly_sorted_unique(&self.capabilities)
            || self.staging_class != CanaryStagingClass::Persistent
            || !safe_persistent_macos_path(&self.staging_root)
            || self.artifact_bytes_total == 0
            || required_free.is_none_or(|required| self.free_bytes < required)
            || self.observed_at_ms == 0
            || self.model_calls != 0
        {
            return Err(CacheObserverError::Invalid(
                "authenticated remote M1 cache authority".to_owned(),
            ));
        }
        Ok(())
    }

    /// Domain-separated digest of the complete authority.
    pub fn digest(&self) -> Result<Sha256Digest, CacheObserverError> {
        self.validate()?;
        domain_digest("shipyard.pulp-mac-cache.remote-authority.v1", self)
    }
}

/// Carrier-origin counters for one exact companion request/response exchange.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteM1CacheTransportStats {
    /// Exact authenticated route used by the successful companion exchange.
    pub route: CanaryRoute,
    /// Direct-LAN authentication probe RTT when attempted.
    pub lan_probe_round_trip_ms: Option<u64>,
    /// Tailnet authentication probe RTT when fallback was attempted.
    pub tailnet_probe_round_trip_ms: Option<u64>,
    /// Stable reason the LAN attempt did not carry the request.
    pub fallback_class: Option<RemoteM1CacheCarrierFailureClass>,
    /// Digest of the exact request bytes sent to M1.
    pub request_sha256: Sha256Digest,
    /// Digest of the exact response bytes received from M1.
    pub response_sha256: Sha256Digest,
    /// Exact request bytes reported by the protected carrier.
    pub request_bytes_sent: u64,
    /// Exact response bytes reported by the protected carrier.
    pub response_bytes_received: u64,
    /// Controller-monotonic round-trip time.
    pub round_trip_ms: u64,
}

/// Redacted bounded carrier failure used for fallback and interruption receipts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteM1CacheCarrierFailureClass {
    /// The route could not be opened or disappeared before completion.
    Unavailable,
    /// The absolute command deadline elapsed.
    TimedOut,
    /// The supervised process failed while a request was in flight.
    Interrupted,
    /// The authenticated remote endpoint refused the fixed companion command.
    RemoteRefused,
    /// Captured output exceeded the protocol's fixed bounds.
    OutputLimit,
}

impl RemoteM1CacheCarrierFailureClass {
    const fn code(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::TimedOut => "timed_out",
            Self::Interrupted => "interrupted",
            Self::RemoteRefused => "remote_refused",
            Self::OutputLimit => "output_limit",
        }
    }

    const fn permits_tailnet_fallback(self) -> bool {
        matches!(self, Self::Unavailable | Self::TimedOut | Self::Interrupted)
    }
}

/// Complete authority attached to one M1 cache-generation receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteM1CacheAuthorityReceipt {
    /// Receipt schema.
    pub schema_version: u32,
    /// Pre-invocation authenticated authority.
    pub authority: RemoteM1CacheAuthority,
    /// Exact carrier-origin request/response counters.
    pub transport: RemoteM1CacheTransportStats,
    /// M1 wall-clock timestamp retained only to authenticate response bytes.
    pub remote_observed_at_ms: u64,
    /// Routine observation uses no model.
    pub model_calls: u64,
}

impl RemoteM1CacheAuthorityReceipt {
    /// Validate intrinsic receipt integrity.
    pub fn validate(&self) -> Result<(), CacheObserverError> {
        self.authority.validate()?;
        if self.schema_version != REMOTE_M1_CACHE_PROTOCOL_SCHEMA
            || self.transport.request_bytes_sent == 0
            || self.transport.response_bytes_received == 0
            || self.transport.round_trip_ms == 0
            || self.transport.route != self.authority.route
            || !valid_route_measurements(&self.transport)
            || self.remote_observed_at_ms == 0
            || self.model_calls != 0
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 cache authority receipt".to_owned(),
            ));
        }
        Ok(())
    }

    /// Domain-separated digest of this complete receipt.
    pub fn digest(&self) -> Result<Sha256Digest, CacheObserverError> {
        self.validate()?;
        domain_digest("shipyard.pulp-mac-cache.remote-receipt.v1", self)
    }

    /// Whether this receipt supplies every remote M1 controller fence.
    #[must_use]
    pub fn proves(
        &self,
        host_observation_sha256: &Sha256Digest,
        artifact_bytes_total: u64,
        assessed_at_ms: u64,
        maximum_age_ms: u64,
    ) -> bool {
        self.validate().is_ok()
            && &self.authority.host_observation_sha256 == host_observation_sha256
            && self.authority.artifact_bytes_total == artifact_bytes_total
            && assessed_at_ms >= self.authority.observed_at_ms
            && assessed_at_ms.saturating_sub(self.authority.observed_at_ms) <= maximum_age_ms
    }

    pub(crate) fn has_same_controller_fence(&self, other: &Self) -> bool {
        self.authority.host_id == other.authority.host_id
            && self.authority.source_host_id == other.authority.source_host_id
            && self.authority.host_observation_sha256 == other.authority.host_observation_sha256
            && self.authority.host_session_generation == other.authority.host_session_generation
            && self.authority.route == other.authority.route
            && self.authority.destination == other.authority.destination
            && self.authority.known_hosts_sha256 == other.authority.known_hosts_sha256
            && self.authority.capabilities == other.authority.capabilities
            && self.authority.staging_root == other.authority.staging_root
            && self.authority.staging_class == other.authority.staging_class
            && self.authority.artifact_bytes_total == other.authority.artifact_bytes_total
            && self.authority.minimum_reserve_bytes == other.authority.minimum_reserve_bytes
            && self.authority.terminal_instance_sha256 == other.authority.terminal_instance_sha256
            && self.authority.companion_executable_sha256
                == other.authority.companion_executable_sha256
    }

    pub(crate) fn binds_cache_observation(
        &self,
        host_observation_sha256: &Sha256Digest,
        cache_root: &str,
        manifest: &CacheGenerationManifest,
        probe_elapsed_ms: u64,
    ) -> Result<bool, CacheObserverError> {
        self.validate()?;
        if &self.authority.host_observation_sha256 != host_observation_sha256 {
            return Ok(false);
        }
        let request = RemoteM1CacheRequest::from_parts(cache_root, manifest, &self.authority)?;
        let request_bytes = serde_json::to_vec(&request)?;
        let response = RemoteM1CacheResponse {
            schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
            request_sha256: request.digest()?,
            authority_sha256: request.authority_sha256.clone(),
            host_id: request.host_id.clone(),
            observed_at_ms: self.remote_observed_at_ms,
            probe_elapsed_ms,
            cache_root: cache_root.to_owned(),
            manifest: manifest.clone(),
            manifest_sha256: manifest.digest()?,
            companion_executable_sha256: request.companion_executable_sha256.clone(),
            model_calls: 0,
        };
        let response_bytes = serde_json::to_vec(&response)?;
        Ok(
            self.transport.request_sha256 == Sha256Digest::of_bytes(&request_bytes)
                && self.transport.request_bytes_sent == request_bytes.len() as u64
                && self.transport.response_sha256 == Sha256Digest::of_bytes(&response_bytes)
                && self.transport.response_bytes_received == response_bytes.len() as u64,
        )
    }
}

/// Exact bytes and carrier measurements returned by the protected transport.
#[derive(Debug)]
pub struct RemoteM1CacheTransportOutput {
    /// Strict companion response bytes.
    pub response: Vec<u8>,
    /// Carrier-origin byte and monotonic timing counters.
    pub stats: RemoteM1CacheTransportStats,
}

/// Protected M3-to-M1 invocation boundary.
///
/// Implementations must use the existing authenticated companion/terminal
/// channel. They must not fall back to direct SSH, ambient configuration, or a
/// worker-authored authority record.
pub trait RemoteM1CacheTransport {
    /// Re-observe the exact protected host/session/LAN/capability/storage authority.
    fn authenticate_m1(
        &mut self,
        deadline: Instant,
    ) -> Result<RemoteM1CacheAuthority, CacheObserverError>;

    /// Invoke the digest-pinned companion with the exact canonical request.
    fn invoke_cache_observer(
        &mut self,
        request: &[u8],
        deadline: Instant,
    ) -> Result<RemoteM1CacheTransportOutput, CacheObserverError>;
}

#[derive(Debug)]
pub(crate) struct RemoteM1CacheCommandOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    elapsed_ms: u64,
}

/// Injectable supervised process boundary for the production strict-SSH carrier.
pub(crate) trait RemoteM1CacheCommandRunner {
    /// Run one already-authenticated SSH command with exact stdin and bounded output.
    fn run(
        &mut self,
        command: &mut Command,
        request: &[u8],
        deadline: Instant,
        maximum_stdout_bytes: u64,
    ) -> Result<RemoteM1CacheCommandOutput, RemoteM1CacheCarrierFailureClass>;
}

/// Descendant-safe production runner used by the strict-SSH cache carrier.
#[derive(Clone, Copy, Debug, Default)]
struct SystemRemoteM1CacheCommandRunner;

impl RemoteM1CacheCommandRunner for SystemRemoteM1CacheCommandRunner {
    fn run(
        &mut self,
        command: &mut Command,
        request: &[u8],
        deadline: Instant,
        maximum_stdout_bytes: u64,
    ) -> Result<RemoteM1CacheCommandOutput, RemoteM1CacheCarrierFailureClass> {
        if Instant::now() >= deadline {
            return Err(RemoteM1CacheCarrierFailureClass::TimedOut);
        }
        let mut stdin =
            tempfile::tempfile().map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?;
        stdin
            .write_all(request)
            .and_then(|()| stdin.seek(SeekFrom::Start(0)).map(drop))
            .map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?;
        let mut stdout =
            tempfile::tempfile().map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?;
        let stderr =
            tempfile::tempfile().map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?;
        command
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(
                stdout
                    .try_clone()
                    .map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?,
            ))
            .stderr(Stdio::from(
                stderr
                    .try_clone()
                    .map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?,
            ));
        let started = Instant::now();
        let mut tree = ProcessTree::spawn(command)
            .map_err(|_| RemoteM1CacheCarrierFailureClass::Unavailable)?;
        let status = loop {
            if stdout
                .metadata()
                .map_err(|_| RemoteM1CacheCarrierFailureClass::Interrupted)?
                .len()
                > maximum_stdout_bytes
                || stderr
                    .metadata()
                    .map_err(|_| RemoteM1CacheCarrierFailureClass::Interrupted)?
                    .len()
                    > MAX_COMPANION_STDERR_BYTES
            {
                tree.terminate_until(deadline);
                return Err(RemoteM1CacheCarrierFailureClass::OutputLimit);
            }
            match tree.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => std::thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                ),
                Ok(None) => {
                    tree.terminate_until(deadline);
                    return Err(RemoteM1CacheCarrierFailureClass::TimedOut);
                }
                Err(_) => {
                    tree.terminate_until(deadline);
                    return Err(RemoteM1CacheCarrierFailureClass::Interrupted);
                }
            }
        };
        tree.terminate_until(deadline);
        stdout
            .seek(SeekFrom::Start(0))
            .map_err(|_| RemoteM1CacheCarrierFailureClass::Interrupted)?;
        let mut response = Vec::new();
        (&mut stdout)
            .take(maximum_stdout_bytes + 1)
            .read_to_end(&mut response)
            .map_err(|_| RemoteM1CacheCarrierFailureClass::Interrupted)?;
        if response.len() as u64 > maximum_stdout_bytes {
            return Err(RemoteM1CacheCarrierFailureClass::OutputLimit);
        }
        Ok(RemoteM1CacheCommandOutput {
            status,
            stdout: response,
            elapsed_ms: milliseconds_ceil(started.elapsed())
                .map_err(|_| RemoteM1CacheCarrierFailureClass::Interrupted)?,
        })
    }
}

#[derive(Clone)]
struct SelectedStrictSshRoute {
    target: StrictSshCanaryTarget,
    authority: RemoteM1CacheAuthority,
    lan_probe_round_trip_ms: Option<u64>,
    tailnet_probe_round_trip_ms: Option<u64>,
    fallback_class: Option<RemoteM1CacheCarrierFailureClass>,
}

/// Production-callable M3-to-M1 carrier over explicit pinned strict-SSH routes.
///
/// The LAN target is always tried first. Tailnet is an optional measured
/// fallback only for transport availability failures; authenticated remote
/// refusal never reroutes. Both targets reject ambient SSH config and agents.
pub struct StrictSshRemoteM1CacheTransport {
    lan: StrictSshCanaryTarget,
    tailnet: Option<StrictSshCanaryTarget>,
    authority_template: RemoteM1CacheAuthority,
    companion_path: String,
    route_probe_timeout: Duration,
    runner: Box<dyn RemoteM1CacheCommandRunner + Send>,
    selected: Option<SelectedStrictSshRoute>,
}

impl StrictSshRemoteM1CacheTransport {
    /// Construct the production carrier. No network request occurs here.
    pub fn new(
        lan: StrictSshCanaryTarget,
        tailnet: Option<StrictSshCanaryTarget>,
        authority_template: RemoteM1CacheAuthority,
        companion_path: impl Into<String>,
        route_probe_timeout: Duration,
    ) -> Result<Self, CacheObserverError> {
        Self::with_runner(
            lan,
            tailnet,
            authority_template,
            companion_path,
            route_probe_timeout,
            SystemRemoteM1CacheCommandRunner,
        )
    }

    /// Inject a deterministic process boundary for focused carrier tests.
    pub(crate) fn with_runner<R: RemoteM1CacheCommandRunner + Send + 'static>(
        lan: StrictSshCanaryTarget,
        tailnet: Option<StrictSshCanaryTarget>,
        authority_template: RemoteM1CacheAuthority,
        companion_path: impl Into<String>,
        route_probe_timeout: Duration,
        runner: R,
    ) -> Result<Self, CacheObserverError> {
        authority_template.validate()?;
        let companion_path = companion_path.into();
        if route_probe_timeout.is_zero()
            || route_probe_timeout > Duration::from_secs(15)
            || !safe_remote_program(&companion_path)
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 strict-SSH carrier configuration".to_owned(),
            ));
        }
        Ok(Self {
            lan,
            tailnet,
            authority_template,
            companion_path,
            route_probe_timeout,
            runner: Box::new(runner),
            selected: None,
        })
    }

    fn probe(
        &mut self,
        target: &StrictSshCanaryTarget,
        route: CanaryRoute,
        deadline: Instant,
    ) -> Result<(RemoteM1CacheAuthority, u64), (RemoteM1CacheCarrierFailureClass, u64)> {
        let started = Instant::now();
        if started >= deadline {
            return Err((RemoteM1CacheCarrierFailureClass::TimedOut, 0));
        }
        let mut invocation = target
            .prepare_remote_command("/usr/bin/true", &[])
            .map_err(|_| (RemoteM1CacheCarrierFailureClass::RemoteRefused, 0))?;
        let probe_deadline = deadline.min(started + self.route_probe_timeout);
        let output = self
            .runner
            .run(&mut invocation.command, &[], probe_deadline, 1024);
        invocation
            .verify_unchanged()
            .map_err(|_| (RemoteM1CacheCarrierFailureClass::RemoteRefused, 0))?;
        let elapsed = milliseconds_ceil(started.elapsed()).unwrap_or(1);
        let output = output.map_err(|class| (class, elapsed))?;
        if !output.status.success() {
            return Err((
                if output.status.code() == Some(255) {
                    RemoteM1CacheCarrierFailureClass::Interrupted
                } else {
                    RemoteM1CacheCarrierFailureClass::RemoteRefused
                },
                output.elapsed_ms,
            ));
        }
        if !output.stdout.is_empty() {
            return Err((
                RemoteM1CacheCarrierFailureClass::RemoteRefused,
                output.elapsed_ms,
            ));
        }
        let mut authority = self.authority_template.clone();
        authority.route = route;
        authority.destination = invocation.destination;
        authority.known_hosts_sha256 = invocation.known_hosts_sha256;
        authority.validate().map_err(|_| {
            (
                RemoteM1CacheCarrierFailureClass::RemoteRefused,
                output.elapsed_ms,
            )
        })?;
        Ok((authority, output.elapsed_ms))
    }
}

impl RemoteM1CacheTransport for StrictSshRemoteM1CacheTransport {
    fn authenticate_m1(
        &mut self,
        deadline: Instant,
    ) -> Result<RemoteM1CacheAuthority, CacheObserverError> {
        let lan = self.lan.clone();
        match self.probe(&lan, CanaryRoute::Lan, deadline) {
            Ok((authority, elapsed_ms)) => {
                self.selected = Some(SelectedStrictSshRoute {
                    target: lan,
                    authority: authority.clone(),
                    lan_probe_round_trip_ms: Some(elapsed_ms),
                    tailnet_probe_round_trip_ms: None,
                    fallback_class: None,
                });
                Ok(authority)
            }
            Err((class, lan_elapsed_ms)) if class.permits_tailnet_fallback() => {
                let tailnet = self.tailnet.clone().ok_or_else(|| carrier_error(class))?;
                let (authority, elapsed_ms) = self
                    .probe(&tailnet, CanaryRoute::Tailnet, deadline)
                    .map_err(|(failure, _)| carrier_error(failure))?;
                self.selected = Some(SelectedStrictSshRoute {
                    target: tailnet,
                    authority: authority.clone(),
                    lan_probe_round_trip_ms: Some(lan_elapsed_ms),
                    tailnet_probe_round_trip_ms: Some(elapsed_ms),
                    fallback_class: Some(class),
                });
                Ok(authority)
            }
            Err((class, _)) => Err(carrier_error(class)),
        }
    }

    fn invoke_cache_observer(
        &mut self,
        request: &[u8],
        deadline: Instant,
    ) -> Result<RemoteM1CacheTransportOutput, CacheObserverError> {
        if request.is_empty() || request.len() as u64 > MAX_COMPANION_MESSAGE_BYTES {
            return Err(carrier_error(RemoteM1CacheCarrierFailureClass::OutputLimit));
        }
        let selected = self.selected.clone().ok_or_else(|| {
            CacheObserverError::Invalid(
                "remote M1 strict-SSH route is not authenticated".to_owned(),
            )
        })?;
        let mut invocation = selected
            .target
            .prepare_remote_command(&self.companion_path, &["--observe-m1-cache"])
            .map_err(|_| carrier_error(RemoteM1CacheCarrierFailureClass::Unavailable))?;
        if invocation.destination != selected.authority.destination
            || invocation.known_hosts_sha256 != selected.authority.known_hosts_sha256
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 strict-SSH authority drift".to_owned(),
            ));
        }
        let output = self.runner.run(
            &mut invocation.command,
            request,
            deadline,
            MAX_COMPANION_MESSAGE_BYTES,
        );
        invocation.verify_unchanged().map_err(|_| {
            CacheObserverError::Invalid("remote M1 host-key authority drift".to_owned())
        })?;
        let output = output.map_err(carrier_error)?;
        if !output.status.success() {
            return Err(carrier_error(if output.status.code() == Some(255) {
                RemoteM1CacheCarrierFailureClass::Interrupted
            } else {
                RemoteM1CacheCarrierFailureClass::RemoteRefused
            }));
        }
        Ok(RemoteM1CacheTransportOutput {
            stats: RemoteM1CacheTransportStats {
                route: selected.authority.route,
                lan_probe_round_trip_ms: selected.lan_probe_round_trip_ms,
                tailnet_probe_round_trip_ms: selected.tailnet_probe_round_trip_ms,
                fallback_class: selected.fallback_class,
                request_sha256: Sha256Digest::of_bytes(request),
                response_sha256: Sha256Digest::of_bytes(&output.stdout),
                request_bytes_sent: request.len() as u64,
                response_bytes_received: output.stdout.len() as u64,
                round_trip_ms: output.elapsed_ms,
            },
            response: output.stdout,
        })
    }
}

/// Production-shape remote observer over a caller-owned authenticated transport.
pub struct AuthenticatedRemoteM1CacheObserver<T> {
    transport: T,
    source_host_id: String,
    worker_host_id: String,
    timeout: Duration,
    maximum_authority_age_ms: u64,
}

impl<T: RemoteM1CacheTransport> AuthenticatedRemoteM1CacheObserver<T> {
    /// Construct a bounded observer. No transport call occurs here.
    pub fn new(
        transport: T,
        source_host_id: impl Into<String>,
        worker_host_id: impl Into<String>,
        timeout: Duration,
        maximum_authority_age_ms: u64,
    ) -> Result<Self, CacheObserverError> {
        let source_host_id = source_host_id.into();
        let worker_host_id = worker_host_id.into();
        if !valid_host_id(&source_host_id)
            || !valid_host_id(&worker_host_id)
            || source_host_id == worker_host_id
            || timeout.is_zero()
            || timeout > Duration::from_secs(30)
            || maximum_authority_age_ms == 0
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 cache observer bounds".to_owned(),
            ));
        }
        Ok(Self {
            transport,
            source_host_id,
            worker_host_id,
            timeout,
            maximum_authority_age_ms,
        })
    }

    /// Authenticate and observe one exact immutable worker cache generation.
    pub fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        if spec.host_id() != self.worker_host_id {
            return Err(CacheObserverError::Invalid(
                "remote cache observer worker binding".to_owned(),
            ));
        }
        let deadline = Instant::now() + self.timeout;
        let authority = self.transport.authenticate_m1(deadline)?;
        authority.validate()?;
        let now = controller_now_ms()?;
        if authority.source_host_id != self.source_host_id
            || authority.host_id != self.worker_host_id
            || authority.host_observation_sha256 != *spec.host_observation_sha256()
            || authority.observed_at_ms > now
            || now.saturating_sub(authority.observed_at_ms) > self.maximum_authority_age_ms
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 authority is stale or detached".to_owned(),
            ));
        }
        let request = RemoteM1CacheRequest::new(spec, &authority)?;
        let request_bytes = serde_json::to_vec(&request)?;
        let expected_request_sha256 = Sha256Digest::of_bytes(&request_bytes);
        let output = self
            .transport
            .invoke_cache_observer(&request_bytes, deadline)?;
        validate_transport_stats(
            &output,
            &expected_request_sha256,
            request_bytes.len(),
            authority.route,
        )?;
        let response: RemoteM1CacheResponse = bounded_json(&output.response)?;
        response.validate(&request)?;
        let controller_observed_at_ms = controller_now_ms()?;
        if controller_observed_at_ms < now {
            return Err(CacheObserverError::Invalid(
                "controller clock moved backward during cache observation".to_owned(),
            ));
        }
        let receipt = CacheGenerationObservationReceipt {
            schema_version: crate::parallel_proof_canary_cache::CACHE_GENERATION_OBSERVATION_SCHEMA,
            host_id: self.worker_host_id.clone(),
            observed_at_ms: controller_observed_at_ms,
            probe_elapsed_ms: response.probe_elapsed_ms,
            host_observation_sha256: authority.host_observation_sha256.clone(),
            cache_root: response.cache_root,
            manifest_sha256: response.manifest.digest()?,
            manifest: response.manifest,
            remote_authority: Some(RemoteM1CacheAuthorityReceipt {
                schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
                authority,
                transport: output.stats,
                remote_observed_at_ms: response.observed_at_ms,
                model_calls: 0,
            }),
            model_calls: 0,
        };
        receipt.validate()?;
        Ok(receipt)
    }
}

/// Exact role router used by the builder-before-worker paired cache driver.
pub struct PairedAuthenticatedCacheObserver<T> {
    local: LocalCacheGenerationObserver,
    remote: AuthenticatedRemoteM1CacheObserver<T>,
    builder_host_id: String,
    worker_host_id: String,
}

impl<T: RemoteM1CacheTransport> PairedAuthenticatedCacheObserver<T> {
    /// Pair one explicit local builder with one authenticated remote worker.
    pub fn new(
        builder_host_id: impl Into<String>,
        worker_host_id: impl Into<String>,
        remote: AuthenticatedRemoteM1CacheObserver<T>,
    ) -> Result<Self, CacheObserverError> {
        let builder_host_id = builder_host_id.into();
        let worker_host_id = worker_host_id.into();
        if builder_host_id == worker_host_id || remote.worker_host_id != worker_host_id {
            return Err(CacheObserverError::Invalid(
                "paired cache observer host binding".to_owned(),
            ));
        }
        Ok(Self {
            local: LocalCacheGenerationObserver::new(builder_host_id.clone())?,
            remote,
            builder_host_id,
            worker_host_id,
        })
    }
}

impl<T: RemoteM1CacheTransport> CacheGenerationObserver for PairedAuthenticatedCacheObserver<T> {
    fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        if spec.host_id() == self.builder_host_id {
            self.local.observe(spec)
        } else if spec.host_id() == self.worker_host_id {
            self.remote.observe(spec)
        } else {
            Err(CacheObserverError::Invalid(
                "paired cache observer host binding".to_owned(),
            ))
        }
    }

    fn controller_now_ms(&mut self) -> Result<u64, CacheObserverError> {
        controller_now_ms()
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteM1CacheRequest {
    schema_version: u32,
    host_id: String,
    authority_sha256: Sha256Digest,
    host_observation_sha256: Sha256Digest,
    terminal_instance_sha256: Sha256Digest,
    companion_executable_sha256: Sha256Digest,
    cache_root: String,
    expected_manifest: CacheGenerationManifest,
    expected_manifest_sha256: Sha256Digest,
    model_calls: u64,
}

impl RemoteM1CacheRequest {
    fn new(
        spec: &CacheGenerationProbeSpec,
        authority: &RemoteM1CacheAuthority,
    ) -> Result<Self, CacheObserverError> {
        Self::from_parts(
            &spec.root().to_string_lossy(),
            spec.expected_manifest(),
            authority,
        )
    }

    fn from_parts(
        cache_root: &str,
        expected_manifest: &CacheGenerationManifest,
        authority: &RemoteM1CacheAuthority,
    ) -> Result<Self, CacheObserverError> {
        let request = Self {
            schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
            host_id: authority.host_id.clone(),
            authority_sha256: authority.digest()?,
            host_observation_sha256: authority.host_observation_sha256.clone(),
            terminal_instance_sha256: authority.terminal_instance_sha256.clone(),
            companion_executable_sha256: authority.companion_executable_sha256.clone(),
            cache_root: cache_root.to_owned(),
            expected_manifest_sha256: expected_manifest.digest()?,
            expected_manifest: expected_manifest.clone(),
            model_calls: 0,
        };
        request.validate()?;
        Ok(request)
    }

    fn validate(&self) -> Result<(), CacheObserverError> {
        self.expected_manifest.validate()?;
        if self.schema_version != REMOTE_M1_CACHE_PROTOCOL_SCHEMA
            || !valid_host_id(&self.host_id)
            || self.model_calls != 0
            || !safe_persistent_macos_path(&self.cache_root)
            || self.expected_manifest.digest()? != self.expected_manifest_sha256
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 cache companion request".to_owned(),
            ));
        }
        Ok(())
    }

    fn digest(&self) -> Result<Sha256Digest, CacheObserverError> {
        self.validate()?;
        domain_digest("shipyard.pulp-mac-cache.remote-request.v1", self)
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RemoteM1CacheResponse {
    schema_version: u32,
    request_sha256: Sha256Digest,
    authority_sha256: Sha256Digest,
    host_id: String,
    observed_at_ms: u64,
    probe_elapsed_ms: u64,
    cache_root: String,
    manifest: CacheGenerationManifest,
    manifest_sha256: Sha256Digest,
    companion_executable_sha256: Sha256Digest,
    model_calls: u64,
}

impl RemoteM1CacheResponse {
    fn validate(&self, request: &RemoteM1CacheRequest) -> Result<(), CacheObserverError> {
        self.manifest.validate()?;
        if self.schema_version != REMOTE_M1_CACHE_PROTOCOL_SCHEMA
            || self.request_sha256 != request.digest()?
            || self.authority_sha256 != request.authority_sha256
            || self.host_id != request.host_id
            || self.observed_at_ms == 0
            || self.probe_elapsed_ms == 0
            || self.cache_root != request.cache_root
            || self.manifest != request.expected_manifest
            || self.manifest.digest()? != self.manifest_sha256
            || self.companion_executable_sha256 != request.companion_executable_sha256
            || self.model_calls != 0
        {
            return Err(CacheObserverError::Invalid(
                "remote M1 cache companion response".to_owned(),
            ));
        }
        Ok(())
    }
}

/// Read one strict request on M1 and emit one strict read-only response.
pub fn run_remote_m1_cache_observer_stdio() -> Result<(), String> {
    let request: RemoteM1CacheRequest =
        bounded_json_reader(std::io::stdin().lock()).map_err(|error| error.to_string())?;
    let response = handle_remote_m1_cache_request(&request, verify_current_companion_digest)?;
    let bytes = serde_json::to_vec(&response).map_err(|_| "response serialization refused")?;
    if bytes.len() as u64 > MAX_COMPANION_MESSAGE_BYTES {
        return Err("remote M1 cache response exceeds limit".to_owned());
    }
    let mut stdout = std::io::stdout().lock();
    stdout
        .write_all(&bytes)
        .map_err(|_| "remote M1 cache response output refused".to_owned())
}

fn handle_remote_m1_cache_request(
    request: &RemoteM1CacheRequest,
    verify_companion: impl FnOnce(&Sha256Digest) -> Result<(), String>,
) -> Result<RemoteM1CacheResponse, String> {
    request.validate().map_err(|error| error.to_string())?;
    verify_companion(&request.companion_executable_sha256)?;
    let started = Instant::now();
    let manifest = produce_cache_generation_manifest(
        Path::new(&request.cache_root),
        request.expected_manifest.generation.name.clone(),
        request.expected_manifest.generation.generation.clone(),
    )
    .map_err(|error| error.to_string())?;
    if manifest != request.expected_manifest {
        return Err("remote M1 cache generation mismatch".to_owned());
    }
    Ok(RemoteM1CacheResponse {
        schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
        request_sha256: request.digest().map_err(|error| error.to_string())?,
        authority_sha256: request.authority_sha256.clone(),
        host_id: request.host_id.clone(),
        observed_at_ms: controller_now_ms().map_err(|error| error.to_string())?,
        probe_elapsed_ms: milliseconds_ceil(started.elapsed())
            .map_err(|error| error.to_string())?,
        cache_root: request.cache_root.clone(),
        manifest_sha256: manifest.digest().map_err(|error| error.to_string())?,
        manifest,
        companion_executable_sha256: request.companion_executable_sha256.clone(),
        model_calls: 0,
    })
}

fn validate_transport_stats(
    output: &RemoteM1CacheTransportOutput,
    request_sha256: &Sha256Digest,
    request_bytes: usize,
    expected_route: CanaryRoute,
) -> Result<(), CacheObserverError> {
    if output.response.is_empty()
        || output.response.len() as u64 > MAX_COMPANION_MESSAGE_BYTES
        || output.stats.request_sha256 != *request_sha256
        || output.stats.response_sha256 != Sha256Digest::of_bytes(&output.response)
        || output.stats.request_bytes_sent != request_bytes as u64
        || output.stats.response_bytes_received != output.response.len() as u64
        || output.stats.round_trip_ms == 0
        || output.stats.route != expected_route
        || !valid_route_measurements(&output.stats)
    {
        return Err(CacheObserverError::Invalid(
            "remote M1 cache transport counters".to_owned(),
        ));
    }
    Ok(())
}

fn valid_route_measurements(stats: &RemoteM1CacheTransportStats) -> bool {
    match stats.route {
        CanaryRoute::Lan => {
            stats
                .lan_probe_round_trip_ms
                .is_some_and(|elapsed| elapsed > 0)
                && stats.tailnet_probe_round_trip_ms.is_none()
                && stats.fallback_class.is_none()
        }
        CanaryRoute::Tailnet => {
            stats
                .lan_probe_round_trip_ms
                .is_some_and(|elapsed| elapsed > 0)
                && stats
                    .tailnet_probe_round_trip_ms
                    .is_some_and(|elapsed| elapsed > 0)
                && stats
                    .fallback_class
                    .is_some_and(RemoteM1CacheCarrierFailureClass::permits_tailnet_fallback)
        }
        _ => false,
    }
}

fn carrier_error(class: RemoteM1CacheCarrierFailureClass) -> CacheObserverError {
    CacheObserverError::Invalid(format!("remote M1 cache carrier {}", class.code()))
}

fn safe_destination(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.len() <= 512
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'@' | b':')
        })
}

fn safe_remote_program(value: &str) -> bool {
    value.starts_with('/')
        && value != "/"
        && value.len() <= 1024
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'-' | b'_'))
        && value
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn bounded_json<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, CacheObserverError> {
    if bytes.is_empty() || bytes.len() as u64 > MAX_COMPANION_MESSAGE_BYTES {
        return Err(CacheObserverError::Invalid(
            "remote M1 cache companion message size".to_owned(),
        ));
    }
    Ok(serde_json::from_slice(bytes)?)
}

fn bounded_json_reader<T: for<'de> Deserialize<'de>>(
    reader: impl Read,
) -> Result<T, CacheObserverError> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_COMPANION_MESSAGE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    bounded_json(&bytes)
}

fn domain_digest<T: Serialize>(
    domain: &str,
    value: &T,
) -> Result<Sha256Digest, CacheObserverError> {
    let payload = serde_json::to_vec(value)?;
    let mut bytes = Vec::with_capacity(domain.len() + payload.len() + 16);
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(Sha256Digest::of_bytes(&bytes))
}

fn strictly_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn valid_host_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn safe_persistent_macos_path(value: &str) -> bool {
    value.starts_with('/')
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

fn controller_now_ms() -> Result<u64, CacheObserverError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CacheObserverError::Invalid(error.to_string()))?
        .as_millis();
    u64::try_from(millis).map_err(|_| CacheObserverError::Invalid("clock overflow".to_owned()))
}

fn milliseconds_ceil(duration: Duration) -> Result<u64, CacheObserverError> {
    let millis = duration.as_millis();
    let millis = if duration.subsec_nanos().is_multiple_of(1_000_000) {
        millis
    } else {
        millis
            .checked_add(1)
            .ok_or_else(|| CacheObserverError::Invalid("duration overflow".to_owned()))?
    };
    u64::try_from(millis).map_err(|_| CacheObserverError::Invalid("duration overflow".to_owned()))
}

#[cfg(test)]
pub(crate) fn test_remote_authority_receipt(
    authority: RemoteM1CacheAuthority,
    cache_root: &str,
    manifest: &CacheGenerationManifest,
    observed_at_ms: u64,
    probe_elapsed_ms: u64,
) -> RemoteM1CacheAuthorityReceipt {
    let route = authority.route;
    let request = RemoteM1CacheRequest::from_parts(cache_root, manifest, &authority).unwrap();
    let request_bytes = serde_json::to_vec(&request).unwrap();
    let response = RemoteM1CacheResponse {
        schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
        request_sha256: request.digest().unwrap(),
        authority_sha256: request.authority_sha256,
        host_id: request.host_id,
        observed_at_ms,
        probe_elapsed_ms,
        cache_root: cache_root.to_owned(),
        manifest: manifest.clone(),
        manifest_sha256: manifest.digest().unwrap(),
        companion_executable_sha256: request.companion_executable_sha256,
        model_calls: 0,
    };
    let response_bytes = serde_json::to_vec(&response).unwrap();
    RemoteM1CacheAuthorityReceipt {
        schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
        authority,
        transport: RemoteM1CacheTransportStats {
            route,
            lan_probe_round_trip_ms: Some(1),
            tailnet_probe_round_trip_ms: (route == CanaryRoute::Tailnet).then_some(2),
            fallback_class: (route == CanaryRoute::Tailnet)
                .then_some(RemoteM1CacheCarrierFailureClass::Unavailable),
            request_sha256: Sha256Digest::of_bytes(&request_bytes),
            response_sha256: Sha256Digest::of_bytes(&response_bytes),
            request_bytes_sent: request_bytes.len() as u64,
            response_bytes_received: response_bytes.len() as u64,
            round_trip_ms: 1,
        },
        remote_observed_at_ms: observed_at_ms,
        model_calls: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    use std::sync::{Arc, Mutex};

    use tempfile::TempDir;

    use super::*;
    use crate::parallel_proof_canary::PulpMacCanaryPolicy;
    use crate::parallel_proof_canary_cache::{
        PulpMacCacheEvidenceStore, PulpMacCacheProbeRequest, drive_pulp_mac_cache_probe,
    };

    fn persistent_temp() -> TempDir {
        tempfile::Builder::new()
            .prefix(".shipyard-remote-cache-test-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap()
    }

    fn cache_tree() -> TempDir {
        let root = persistent_temp();
        fs::create_dir(root.path().join("objects")).unwrap();
        fs::write(root.path().join("objects/cache.bin"), b"immutable-cache").unwrap();
        root
    }

    fn digest(label: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(label.as_bytes())
    }

    fn authority(host_observation_sha256: Sha256Digest) -> RemoteM1CacheAuthority {
        RemoteM1CacheAuthority {
            source_host_id: "m3".to_owned(),
            host_id: "m1".to_owned(),
            host_observation_sha256,
            host_session_generation: 12,
            route: CanaryRoute::Lan,
            destination: "shipyard@m1.local".to_owned(),
            known_hosts_sha256: digest("known-hosts"),
            capabilities: vec!["macos-arm64".to_owned()],
            staging_root: "/Users/test/shipyard-staging".to_owned(),
            staging_class: CanaryStagingClass::Persistent,
            free_bytes: 10_000,
            artifact_bytes_total: 1_000,
            minimum_reserve_bytes: 1_000,
            terminal_instance_sha256: digest("verified-terminal-instance"),
            companion_executable_sha256: digest("paired-companion"),
            observed_at_ms: controller_now_ms().unwrap(),
            model_calls: 0,
        }
    }

    #[cfg(unix)]
    #[derive(Clone, Debug)]
    enum FakeCommandStep {
        Success { stdout: Vec<u8>, elapsed_ms: u64 },
        Companion { elapsed_ms: u64 },
        Failure(RemoteM1CacheCarrierFailureClass),
    }

    #[cfg(unix)]
    #[derive(Clone, Debug)]
    struct RecordedCommand {
        arguments: Vec<String>,
        environment: Vec<(String, Option<String>)>,
        request: Vec<u8>,
    }

    #[cfg(unix)]
    #[derive(Clone)]
    struct FakeCommandRunner {
        steps: Arc<Mutex<VecDeque<FakeCommandStep>>>,
        calls: Arc<Mutex<Vec<RecordedCommand>>>,
    }

    #[cfg(unix)]
    impl FakeCommandRunner {
        fn new(steps: impl IntoIterator<Item = FakeCommandStep>) -> Self {
            Self {
                steps: Arc::new(Mutex::new(steps.into_iter().collect())),
                calls: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    #[cfg(unix)]
    impl RemoteM1CacheCommandRunner for FakeCommandRunner {
        fn run(
            &mut self,
            command: &mut Command,
            request: &[u8],
            _deadline: Instant,
            _maximum_stdout_bytes: u64,
        ) -> Result<RemoteM1CacheCommandOutput, RemoteM1CacheCarrierFailureClass> {
            self.calls.lock().unwrap().push(RecordedCommand {
                arguments: command
                    .get_args()
                    .map(|value| value.to_string_lossy().into_owned())
                    .collect(),
                environment: command
                    .get_envs()
                    .map(|(key, value)| {
                        (
                            key.to_string_lossy().into_owned(),
                            value.map(|value| value.to_string_lossy().into_owned()),
                        )
                    })
                    .collect(),
                request: request.to_vec(),
            });
            match self.steps.lock().unwrap().pop_front().unwrap() {
                FakeCommandStep::Success { stdout, elapsed_ms } => Ok(RemoteM1CacheCommandOutput {
                    status: ExitStatus::from_raw(0),
                    stdout,
                    elapsed_ms,
                }),
                FakeCommandStep::Companion { elapsed_ms } => {
                    let request: RemoteM1CacheRequest = serde_json::from_slice(request).unwrap();
                    let response = handle_remote_m1_cache_request(&request, |_| Ok(())).unwrap();
                    Ok(RemoteM1CacheCommandOutput {
                        status: ExitStatus::from_raw(0),
                        stdout: serde_json::to_vec(&response).unwrap(),
                        elapsed_ms,
                    })
                }
                FakeCommandStep::Failure(class) => Err(class),
            }
        }
    }

    #[cfg(unix)]
    fn strict_target(root: &TempDir, label: &str, destination: &str) -> StrictSshCanaryTarget {
        let known_hosts = root.path().join(format!("{label}-known-hosts"));
        let identity = root.path().join(format!("{label}-identity"));
        fs::write(
            &known_hosts,
            format!("{destination} ssh-ed25519 test-key\n"),
        )
        .unwrap();
        fs::write(&identity, b"test-private-key").unwrap();
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
        StrictSshCanaryTarget::new("/usr/bin/ssh", destination, known_hosts, identity, 22).unwrap()
    }

    struct FakeTransport {
        authorities: VecDeque<RemoteM1CacheAuthority>,
        calls: Vec<&'static str>,
        tamper_stats: bool,
        remote_clock_ms: Option<u64>,
    }

    impl RemoteM1CacheTransport for FakeTransport {
        fn authenticate_m1(
            &mut self,
            _deadline: Instant,
        ) -> Result<RemoteM1CacheAuthority, CacheObserverError> {
            self.calls.push("authenticate");
            self.authorities
                .pop_front()
                .ok_or_else(|| CacheObserverError::Invalid("missing fake authority".to_owned()))
        }

        fn invoke_cache_observer(
            &mut self,
            request_bytes: &[u8],
            _deadline: Instant,
        ) -> Result<RemoteM1CacheTransportOutput, CacheObserverError> {
            self.calls.push("invoke");
            let request: RemoteM1CacheRequest = serde_json::from_slice(request_bytes)?;
            let mut response = handle_remote_m1_cache_request(&request, |_| Ok(()))
                .map_err(CacheObserverError::Invalid)?;
            if let Some(remote_clock_ms) = self.remote_clock_ms {
                response.observed_at_ms = remote_clock_ms;
            }
            let response = serde_json::to_vec(&response)?;
            Ok(RemoteM1CacheTransportOutput {
                stats: RemoteM1CacheTransportStats {
                    route: CanaryRoute::Lan,
                    lan_probe_round_trip_ms: Some(1),
                    tailnet_probe_round_trip_ms: None,
                    fallback_class: None,
                    request_sha256: if self.tamper_stats {
                        digest("wrong-request")
                    } else {
                        Sha256Digest::of_bytes(request_bytes)
                    },
                    response_sha256: Sha256Digest::of_bytes(&response),
                    request_bytes_sent: request_bytes.len() as u64,
                    response_bytes_received: response.len() as u64,
                    round_trip_ms: 1,
                },
                response,
            })
        }
    }

    #[test]
    fn remote_observer_binds_every_authority_and_transport_counter() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let host_digest = digest("authenticated-m1-host-observation");
        let spec =
            CacheGenerationProbeSpec::new("m1", host_digest.clone(), root.path(), manifest.clone())
                .unwrap();
        let transport = FakeTransport {
            authorities: VecDeque::from([authority(host_digest.clone())]),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: None,
        };
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            transport,
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        let receipt = observer.observe(&spec).unwrap();
        assert_eq!(receipt.host_id, "m1");
        assert_eq!(receipt.manifest, manifest);
        let remote = receipt.remote_authority.as_ref().unwrap();
        assert_eq!(remote.authority.host_session_generation, 12);
        assert_eq!(remote.authority.route, CanaryRoute::Lan);
        assert_eq!(remote.authority.host_observation_sha256, host_digest);
        assert_eq!(remote.model_calls, 0);
        assert_eq!(observer.transport.calls, ["authenticate", "invoke"]);

        let mut corrupted = receipt;
        corrupted
            .remote_authority
            .as_mut()
            .unwrap()
            .transport
            .response_bytes_received += 1;
        assert!(corrupted.validate().is_err());
    }

    #[test]
    fn remote_observer_uses_explicit_non_pulp_host_pair() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let host_digest = digest("worker-b-observation");
        let spec =
            CacheGenerationProbeSpec::new("worker-b", host_digest.clone(), root.path(), manifest)
                .unwrap();
        let mut observed_authority = authority(host_digest);
        observed_authority.source_host_id = "builder-a".to_owned();
        observed_authority.host_id = "worker-b".to_owned();
        let transport = FakeTransport {
            authorities: VecDeque::from([observed_authority]),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: None,
        };
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            transport,
            "builder-a",
            "worker-b",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        let receipt = observer.observe(&spec).unwrap();
        assert_eq!(receipt.host_id, "worker-b");
        assert_eq!(
            receipt.remote_authority.unwrap().authority.source_host_id,
            "builder-a"
        );
    }

    #[test]
    fn remote_wall_clock_is_authenticated_but_never_used_for_controller_freshness() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let host_digest = digest("authenticated-m1-host-observation");
        let spec = CacheGenerationProbeSpec::new("m1", host_digest.clone(), root.path(), manifest)
            .unwrap();
        let authority = authority(host_digest);
        let authority_time = authority.observed_at_ms;
        let remote_clock_ms = u64::MAX - 1;
        let transport = FakeTransport {
            authorities: VecDeque::from([authority]),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: Some(remote_clock_ms),
        };
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            transport,
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        let receipt = observer.observe(&spec).unwrap();
        assert!(receipt.observed_at_ms >= authority_time);
        assert_ne!(receipt.observed_at_ms, remote_clock_ms);
        assert_eq!(
            receipt
                .remote_authority
                .as_ref()
                .unwrap()
                .remote_observed_at_ms,
            remote_clock_ms
        );
        assert!(receipt.validate().is_ok());
    }

    #[test]
    fn companion_digest_is_verified_before_cache_observation() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let authority = authority(digest("authenticated-m1-host-observation"));
        let request =
            RemoteM1CacheRequest::from_parts(root.path().to_str().unwrap(), &manifest, &authority)
                .unwrap();
        fs::remove_file(root.path().join("objects/cache.bin")).unwrap();

        let error =
            handle_remote_m1_cache_request(
                &request,
                |_| Err("companion digest refused".to_owned()),
            )
            .unwrap_err();
        assert_eq!(error, "companion digest refused");
    }

    #[test]
    fn detached_authority_and_tampered_transport_fail_closed() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let spec = CacheGenerationProbeSpec::new(
            "m1",
            digest("expected-host"),
            root.path(),
            manifest.clone(),
        )
        .unwrap();
        let detached = FakeTransport {
            authorities: VecDeque::from([authority(digest("other-host"))]),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: None,
        };
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            detached,
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        assert!(observer.observe(&spec).is_err());
        assert_eq!(observer.transport.calls, ["authenticate"]);

        let tampered = FakeTransport {
            authorities: VecDeque::from([authority(digest("expected-host"))]),
            calls: Vec::new(),
            tamper_stats: true,
            remote_clock_ms: None,
        };
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            tampered,
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        assert!(observer.observe(&spec).is_err());
        assert_eq!(observer.transport.calls, ["authenticate", "invoke"]);
    }

    #[test]
    fn non_lan_or_insufficient_reserve_authority_is_refused_before_invocation() {
        let root = cache_tree();
        let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
        let host_digest = digest("expected-host");
        let spec = CacheGenerationProbeSpec::new("m1", host_digest.clone(), root.path(), manifest)
            .unwrap();
        let mut invalid = authority(host_digest);
        invalid.route = CanaryRoute::Tailnet;
        invalid.free_bytes = 1;
        let transport = FakeTransport {
            authorities: VecDeque::from([invalid]),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: None,
        };
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            transport,
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        assert!(observer.observe(&spec).is_err());
        assert_eq!(observer.transport.calls, ["authenticate"]);
    }

    #[test]
    fn paired_driver_never_reaches_m1_after_failed_local_m3_proof() {
        let m3_root = cache_tree();
        let m1_root = cache_tree();
        let expected = produce_cache_generation_manifest(m1_root.path(), "skia", "m124").unwrap();
        fs::write(m3_root.path().join("objects/cache.bin"), b"different-cache").unwrap();
        let m3_digest = digest("m3-host");
        let m1_digest = digest("m1-host");
        let request = PulpMacCacheProbeRequest {
            enabled: true,
            correlation_id: "paired-m3-first".to_owned(),
            builder: vec![
                CacheGenerationProbeSpec::new("m3", m3_digest, m3_root.path(), expected.clone())
                    .unwrap(),
            ],
            worker: vec![
                CacheGenerationProbeSpec::new(
                    "m1",
                    m1_digest.clone(),
                    m1_root.path(),
                    expected.clone(),
                )
                .unwrap(),
            ],
        };
        let policy = PulpMacCanaryPolicy {
            enabled: true,
            repository_id: 1_203_111_607,
            repository: "generous-corp/pulp".to_owned(),
            target: "mac".to_owned(),
            target_triple: "aarch64-apple-darwin".to_owned(),
            builder_host_id: "m3".to_owned(),
            worker_host_id: "m1".to_owned(),
            assessed_at_ms: controller_now_ms().unwrap(),
            required_cache_generations: vec![expected.generation.clone()],
            ..PulpMacCanaryPolicy::default()
        };
        let remote = AuthenticatedRemoteM1CacheObserver::new(
            FakeTransport {
                authorities: VecDeque::from([authority(m1_digest)]),
                calls: Vec::new(),
                tamper_stats: false,
                remote_clock_ms: None,
            },
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        let mut observer = PairedAuthenticatedCacheObserver::new("m3", "m1", remote).unwrap();
        let store_parent = persistent_temp();
        let store = PulpMacCacheEvidenceStore::open(store_parent.path().join("evidence")).unwrap();
        assert!(drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).is_err());
        assert!(observer.remote.transport.calls.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn production_carrier_uses_pinned_lan_and_stdin_without_ambient_ssh() {
        let cache = cache_tree();
        let manifest = produce_cache_generation_manifest(cache.path(), "skia", "m124").unwrap();
        let host = digest("m1-host");
        let spec =
            CacheGenerationProbeSpec::new("m1", host.clone(), cache.path(), manifest).unwrap();
        let authorities = persistent_temp();
        let runner = FakeCommandRunner::new([
            FakeCommandStep::Success {
                stdout: Vec::new(),
                elapsed_ms: 3,
            },
            FakeCommandStep::Companion { elapsed_ms: 7 },
        ]);
        let calls = Arc::clone(&runner.calls);
        let transport = StrictSshRemoteM1CacheTransport::with_runner(
            strict_target(&authorities, "lan", "shipyard@m1.local"),
            None,
            authority(host),
            "/usr/local/bin/shipyard-workstream-provider",
            Duration::from_secs(1),
            runner,
        )
        .unwrap();
        let mut observer = AuthenticatedRemoteM1CacheObserver::new(
            transport,
            "m3",
            "m1",
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        let receipt = observer.observe(&spec).unwrap();
        let remote = receipt.remote_authority.unwrap();
        assert_eq!(remote.authority.route, CanaryRoute::Lan);
        assert_eq!(remote.transport.lan_probe_round_trip_ms, Some(3));
        assert_eq!(remote.transport.tailnet_probe_round_trip_ms, None);
        assert_eq!(remote.transport.fallback_class, None);
        assert_eq!(remote.transport.round_trip_ms, 7);
        assert_eq!(remote.model_calls, 0);

        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0].request.is_empty());
        assert!(!calls[1].request.is_empty());
        assert!(calls[1].arguments.windows(2).any(|pair| {
            pair == [
                "/usr/local/bin/shipyard-workstream-provider",
                "--observe-m1-cache",
            ]
        }));
        assert!(
            !calls[1]
                .arguments
                .iter()
                .any(|argument| argument.contains("expected_manifest"))
        );
        assert!(
            calls[1]
                .arguments
                .windows(2)
                .any(|pair| pair == ["-F", "/dev/null"])
        );
        assert!(
            calls[1]
                .arguments
                .iter()
                .any(|argument| argument == "IdentityAgent=none")
        );
        assert_eq!(
            calls[1]
                .environment
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            ["LANG", "LC_ALL", "SHIPYARD_CANARY_KNOWN_HOSTS"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn production_carrier_measures_tailnet_fallback_without_redispatching_request() {
        let authorities = persistent_temp();
        let runner = FakeCommandRunner::new([
            FakeCommandStep::Failure(RemoteM1CacheCarrierFailureClass::Unavailable),
            FakeCommandStep::Success {
                stdout: Vec::new(),
                elapsed_ms: 5,
            },
            FakeCommandStep::Success {
                stdout: b"bounded-response".to_vec(),
                elapsed_ms: 11,
            },
        ]);
        let calls = Arc::clone(&runner.calls);
        let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
            strict_target(&authorities, "lan", "shipyard@m1.local"),
            Some(strict_target(
                &authorities,
                "tailnet",
                "shipyard@m1.tailnet",
            )),
            authority(digest("m1-host")),
            "/usr/local/bin/shipyard-workstream-provider",
            Duration::from_secs(1),
            runner,
        )
        .unwrap();
        let selected = transport
            .authenticate_m1(Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(selected.route, CanaryRoute::Tailnet);
        assert_eq!(selected.destination, "shipyard@m1.tailnet");
        let request = b"bounded-request";
        let output = transport
            .invoke_cache_observer(request, Instant::now() + Duration::from_secs(1))
            .unwrap();
        assert_eq!(output.stats.route, CanaryRoute::Tailnet);
        assert!(output.stats.lan_probe_round_trip_ms.is_some());
        assert_eq!(output.stats.tailnet_probe_round_trip_ms, Some(5));
        assert_eq!(
            output.stats.fallback_class,
            Some(RemoteM1CacheCarrierFailureClass::Unavailable)
        );
        assert_eq!(output.stats.round_trip_ms, 11);
        let calls = calls.lock().unwrap();
        assert_eq!(
            calls.iter().filter(|call| !call.request.is_empty()).count(),
            1
        );
        assert_eq!(calls[2].request, request);
        assert!(
            calls[2]
                .arguments
                .iter()
                .any(|argument| argument == "shipyard@m1.tailnet")
        );
    }

    #[cfg(unix)]
    #[test]
    fn interrupted_companion_transfer_is_classified_and_never_retried() {
        let authorities = persistent_temp();
        let runner = FakeCommandRunner::new([
            FakeCommandStep::Success {
                stdout: Vec::new(),
                elapsed_ms: 2,
            },
            FakeCommandStep::Failure(RemoteM1CacheCarrierFailureClass::Interrupted),
        ]);
        let calls = Arc::clone(&runner.calls);
        let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
            strict_target(&authorities, "lan", "shipyard@m1.local"),
            Some(strict_target(
                &authorities,
                "tailnet",
                "shipyard@m1.tailnet",
            )),
            authority(digest("m1-host")),
            "/usr/local/bin/shipyard-workstream-provider",
            Duration::from_secs(1),
            runner,
        )
        .unwrap();
        transport
            .authenticate_m1(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let error = transport
            .invoke_cache_observer(
                b"request-in-flight",
                Instant::now() + Duration::from_secs(1),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "remote M1 cache carrier interrupted");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].request, b"request-in-flight");
    }

    #[cfg(unix)]
    #[test]
    fn local_ssh_authority_failure_never_falls_back_to_tailnet() {
        let authorities = persistent_temp();
        let lan = strict_target(&authorities, "lan", "shipyard@m1.local");
        fs::set_permissions(
            authorities.path().join("lan-identity"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let runner = FakeCommandRunner::new([FakeCommandStep::Success {
            stdout: Vec::new(),
            elapsed_ms: 1,
        }]);
        let calls = Arc::clone(&runner.calls);
        let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
            lan,
            Some(strict_target(
                &authorities,
                "tailnet",
                "shipyard@m1.tailnet",
            )),
            authority(digest("m1-host")),
            "/usr/local/bin/shipyard-workstream-provider",
            Duration::from_secs(1),
            runner,
        )
        .unwrap();
        let error = transport
            .authenticate_m1(Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.to_string(), "remote M1 cache carrier remote_refused");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn route_measurement_shapes_fail_closed() {
        let baseline = RemoteM1CacheTransportStats {
            route: CanaryRoute::Lan,
            lan_probe_round_trip_ms: Some(1),
            tailnet_probe_round_trip_ms: None,
            fallback_class: None,
            request_sha256: digest("request"),
            response_sha256: digest("response"),
            request_bytes_sent: 1,
            response_bytes_received: 1,
            round_trip_ms: 1,
        };
        assert!(valid_route_measurements(&baseline));
        let mut invalid_lan = baseline.clone();
        invalid_lan.fallback_class = Some(RemoteM1CacheCarrierFailureClass::Unavailable);
        assert!(!valid_route_measurements(&invalid_lan));
        let mut invalid_tailnet = baseline.clone();
        invalid_tailnet.route = CanaryRoute::Tailnet;
        invalid_tailnet.tailnet_probe_round_trip_ms = Some(2);
        invalid_tailnet.fallback_class = Some(RemoteM1CacheCarrierFailureClass::RemoteRefused);
        assert!(!valid_route_measurements(&invalid_tailnet));
        invalid_tailnet.fallback_class = Some(RemoteM1CacheCarrierFailureClass::TimedOut);
        invalid_tailnet.lan_probe_round_trip_ms = None;
        assert!(!valid_route_measurements(&invalid_tailnet));
    }

    #[cfg(unix)]
    #[test]
    fn oversized_request_is_refused_before_companion_invocation() {
        let authorities = persistent_temp();
        let runner = FakeCommandRunner::new([FakeCommandStep::Success {
            stdout: Vec::new(),
            elapsed_ms: 1,
        }]);
        let calls = Arc::clone(&runner.calls);
        let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
            strict_target(&authorities, "lan", "shipyard@m1.local"),
            None,
            authority(digest("m1-host")),
            "/usr/local/bin/shipyard-workstream-provider",
            Duration::from_secs(1),
            runner,
        )
        .unwrap();
        transport
            .authenticate_m1(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let oversized = vec![0_u8; usize::try_from(MAX_COMPANION_MESSAGE_BYTES + 1).unwrap()];
        let error = transport
            .invoke_cache_observer(&oversized, Instant::now() + Duration::from_secs(1))
            .unwrap_err();
        assert_eq!(error.to_string(), "remote M1 cache carrier output_limit");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }
}
