//! Authenticated companion protocol for read-only M1 cache observation.
//!
//! This module deliberately does not select a network carrier. The controller
//! transport trait is the authentication boundary: implementations must bind
//! the already-verified host session, direct-LAN route, capabilities, staging
//! reserve, terminal instance, and companion executable to one invocation.
//! There is no SSH or ambient configuration fallback.

use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::parallel_proof::{MAX_CAPABILITIES, Sha256Digest};
use crate::parallel_proof_canary::{CanaryRoute, CanaryStagingClass};
use crate::parallel_proof_canary_cache::{
    CacheGenerationManifest, CacheGenerationObservationReceipt, CacheGenerationObserver,
    CacheGenerationProbeSpec, CacheObserverError, LocalCacheGenerationObserver,
    produce_cache_generation_manifest,
};
use crate::workstream_provider_adapter::verify_current_companion_digest;

/// Current strict remote-cache companion protocol.
pub const REMOTE_M1_CACHE_PROTOCOL_SCHEMA: u32 = 1;
const MAX_COMPANION_MESSAGE_BYTES: u64 = 20 * 1024 * 1024;

/// Authenticated facts established before invoking the M1 companion.
///
/// Construction validates shape only. Implementations of
/// [`RemoteM1CacheTransport`] own the trust boundary and must populate these
/// fields from protected controller/terminal receipts, never worker claims.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RemoteM1CacheAuthority {
    /// Stable worker host; only `m1` is supported.
    pub host_id: String,
    /// Digest of the exact read-only host receipt used by the controller.
    pub host_observation_sha256: Sha256Digest,
    /// Nonzero reconnect-fenced host session generation.
    pub host_session_generation: u64,
    /// Exact direct-LAN route proven from M3 to M1.
    pub route: CanaryRoute,
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
        if self.host_id != "m1"
            || self.host_session_generation == 0
            || self.route != CanaryRoute::Lan
            || self.capabilities.is_empty()
            || self.capabilities.len() > MAX_CAPABILITIES
            || !strictly_sorted_unique(&self.capabilities)
            || !self
                .capabilities
                .iter()
                .any(|capability| capability == "macos-arm64")
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
            && self.authority.host_observation_sha256 == other.authority.host_observation_sha256
            && self.authority.host_session_generation == other.authority.host_session_generation
            && self.authority.route == other.authority.route
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
        observed_at_ms: u64,
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
            observed_at_ms,
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
    fn authenticate_m1(&mut self) -> Result<RemoteM1CacheAuthority, CacheObserverError>;

    /// Invoke the digest-pinned companion with the exact canonical request.
    fn invoke_cache_observer(
        &mut self,
        request: &[u8],
        deadline: Instant,
    ) -> Result<RemoteM1CacheTransportOutput, CacheObserverError>;
}

/// Production-shape M1 observer over a caller-owned authenticated transport.
pub struct AuthenticatedRemoteM1CacheObserver<T> {
    transport: T,
    timeout: Duration,
    maximum_authority_age_ms: u64,
}

impl<T: RemoteM1CacheTransport> AuthenticatedRemoteM1CacheObserver<T> {
    /// Construct a bounded observer. No transport call occurs here.
    pub fn new(
        transport: T,
        timeout: Duration,
        maximum_authority_age_ms: u64,
    ) -> Result<Self, CacheObserverError> {
        if timeout.is_zero() || timeout > Duration::from_secs(30) || maximum_authority_age_ms == 0 {
            return Err(CacheObserverError::Invalid(
                "remote M1 cache observer bounds".to_owned(),
            ));
        }
        Ok(Self {
            transport,
            timeout,
            maximum_authority_age_ms,
        })
    }

    /// Authenticate and observe one exact immutable M1 cache generation.
    pub fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        if spec.host_id() != "m1" {
            return Err(CacheObserverError::Invalid(
                "remote cache observer only supports m1".to_owned(),
            ));
        }
        let authority = self.transport.authenticate_m1()?;
        authority.validate()?;
        let now = controller_now_ms()?;
        if authority.host_observation_sha256 != *spec.host_observation_sha256()
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
            .invoke_cache_observer(&request_bytes, Instant::now() + self.timeout)?;
        validate_transport_stats(&output, &expected_request_sha256, request_bytes.len())?;
        let response: RemoteM1CacheResponse = bounded_json(&output.response)?;
        response.validate(&request)?;
        let receipt = CacheGenerationObservationReceipt {
            schema_version: crate::parallel_proof_canary_cache::CACHE_GENERATION_OBSERVATION_SCHEMA,
            host_id: "m1".to_owned(),
            observed_at_ms: response.observed_at_ms,
            probe_elapsed_ms: response.probe_elapsed_ms,
            host_observation_sha256: authority.host_observation_sha256.clone(),
            cache_root: response.cache_root,
            manifest_sha256: response.manifest.digest()?,
            manifest: response.manifest,
            remote_authority: Some(RemoteM1CacheAuthorityReceipt {
                schema_version: REMOTE_M1_CACHE_PROTOCOL_SCHEMA,
                authority,
                transport: output.stats,
                model_calls: 0,
            }),
            model_calls: 0,
        };
        receipt.validate()?;
        Ok(receipt)
    }
}

/// Exact role router used by the existing M3-before-M1 paired cache driver.
pub struct PairedAuthenticatedCacheObserver<T> {
    local: LocalCacheGenerationObserver,
    remote: AuthenticatedRemoteM1CacheObserver<T>,
}

impl<T: RemoteM1CacheTransport> PairedAuthenticatedCacheObserver<T> {
    /// Pair the production M3 local observer with one authenticated M1 adapter.
    #[must_use]
    pub fn new(remote: AuthenticatedRemoteM1CacheObserver<T>) -> Self {
        Self {
            local: LocalCacheGenerationObserver,
            remote,
        }
    }
}

impl<T: RemoteM1CacheTransport> CacheGenerationObserver for PairedAuthenticatedCacheObserver<T> {
    fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        match spec.host_id() {
            "m3" => self.local.observe(spec),
            "m1" => self.remote.observe(spec),
            _ => Err(CacheObserverError::Invalid(
                "paired cache observer only supports m3 then m1".to_owned(),
            )),
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
            host_id: "m1".to_owned(),
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
            || self.host_id != "m1"
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
) -> Result<(), CacheObserverError> {
    if output.response.is_empty()
        || output.response.len() as u64 > MAX_COMPANION_MESSAGE_BYTES
        || output.stats.request_sha256 != *request_sha256
        || output.stats.response_sha256 != Sha256Digest::of_bytes(&output.response)
        || output.stats.request_bytes_sent != request_bytes as u64
        || output.stats.response_bytes_received != output.response.len() as u64
        || output.stats.round_trip_ms == 0
    {
        return Err(CacheObserverError::Invalid(
            "remote M1 cache transport counters".to_owned(),
        ));
    }
    Ok(())
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
            request_sha256: Sha256Digest::of_bytes(&request_bytes),
            response_sha256: Sha256Digest::of_bytes(&response_bytes),
            request_bytes_sent: request_bytes.len() as u64,
            response_bytes_received: response_bytes.len() as u64,
            round_trip_ms: 1,
        },
        model_calls: 0,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;

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
            host_id: "m1".to_owned(),
            host_observation_sha256,
            host_session_generation: 12,
            route: CanaryRoute::Lan,
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

    struct FakeTransport {
        authorities: VecDeque<RemoteM1CacheAuthority>,
        calls: Vec<&'static str>,
        tamper_stats: bool,
    }

    impl RemoteM1CacheTransport for FakeTransport {
        fn authenticate_m1(&mut self) -> Result<RemoteM1CacheAuthority, CacheObserverError> {
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
            let response = handle_remote_m1_cache_request(&request, |_| Ok(()))
                .map_err(CacheObserverError::Invalid)?;
            let response = serde_json::to_vec(&response)?;
            Ok(RemoteM1CacheTransportOutput {
                stats: RemoteM1CacheTransportStats {
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
        };
        let mut observer =
            AuthenticatedRemoteM1CacheObserver::new(transport, Duration::from_secs(1), 60_000)
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
        };
        let mut observer =
            AuthenticatedRemoteM1CacheObserver::new(detached, Duration::from_secs(1), 60_000)
                .unwrap();
        assert!(observer.observe(&spec).is_err());
        assert_eq!(observer.transport.calls, ["authenticate"]);

        let tampered = FakeTransport {
            authorities: VecDeque::from([authority(digest("expected-host"))]),
            calls: Vec::new(),
            tamper_stats: true,
        };
        let mut observer =
            AuthenticatedRemoteM1CacheObserver::new(tampered, Duration::from_secs(1), 60_000)
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
        };
        let mut observer =
            AuthenticatedRemoteM1CacheObserver::new(transport, Duration::from_secs(1), 60_000)
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
            assessed_at_ms: controller_now_ms().unwrap(),
            required_cache_generations: vec![expected.generation.clone()],
            ..PulpMacCanaryPolicy::default()
        };
        let remote = AuthenticatedRemoteM1CacheObserver::new(
            FakeTransport {
                authorities: VecDeque::from([authority(m1_digest)]),
                calls: Vec::new(),
                tamper_stats: false,
            },
            Duration::from_secs(1),
            60_000,
        )
        .unwrap();
        let mut observer = PairedAuthenticatedCacheObserver::new(remote);
        let store_parent = persistent_temp();
        let store = PulpMacCacheEvidenceStore::open(store_parent.path().join("evidence")).unwrap();
        assert!(drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).is_err());
        assert!(observer.remote.transport.calls.is_empty());
    }
}
