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
        if builder_host_id == worker_host_id
            || remote.source_host_id != builder_host_id
            || remote.worker_host_id != worker_host_id
        {
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

include!("parallel_proof_canary_remote_cache/runtime.rs");
#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::{
    synthetic_cache_generation_manifest, test_cache_root, test_remote_authority_receipt,
};
#[cfg(test)]
mod tests;
