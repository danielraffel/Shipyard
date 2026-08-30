use super::{
    CacheGenerationManifest, CanaryRoute, REMOTE_M1_CACHE_PROTOCOL_SCHEMA, RemoteM1CacheAuthority,
    RemoteM1CacheAuthorityReceipt, RemoteM1CacheCarrierFailureClass, RemoteM1CacheRequest,
    RemoteM1CacheResponse, RemoteM1CacheTransportStats, Sha256Digest,
};
use crate::parallel_proof_canary::CanaryCacheGeneration;
use crate::parallel_proof_canary_cache::{CACHE_GENERATION_MANIFEST_SCHEMA, CacheGenerationEntry};
use std::path::{Path, PathBuf};

/// A persistent macOS cache root for portable controller/receipt tests.
///
/// Unix tests retain their real fixture so filesystem proofs remain real.
/// Non-Unix tests exercise the portable record and remote protocol with the
/// kind of POSIX path the M1 companion actually owns, rather than incorrectly
/// serializing the Windows controller's local `TempDir` path as an M1 path.
pub(crate) fn test_cache_root(local_root: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        local_root.to_owned()
    }
    #[cfg(not(unix))]
    {
        let _ = local_root;
        PathBuf::from("/Users/test/shipyard-cache")
    }
}

pub(crate) fn synthetic_cache_generation_manifest(
    name: &str,
    generation: &str,
) -> CacheGenerationManifest {
    let contents = b"cache-object";
    let entries = vec![CacheGenerationEntry::File {
        path: "object.bin".to_owned(),
        mode: 0o600,
        size_bytes: contents.len() as u64,
        sha256: Sha256Digest::of_bytes(contents),
    }];
    let content_sha256 = {
        let domain = "shipyard.cache-generation.contents.v1";
        let payload = serde_json::to_vec(&(0o700_u32, &entries)).unwrap();
        let mut bytes = Vec::with_capacity(domain.len() + payload.len() + 16);
        bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
        bytes.extend_from_slice(domain.as_bytes());
        bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
        bytes.extend_from_slice(&payload);
        Sha256Digest::of_bytes(&bytes)
    };
    let manifest = CacheGenerationManifest {
        schema_version: CACHE_GENERATION_MANIFEST_SCHEMA,
        generation: CanaryCacheGeneration {
            name: name.to_owned(),
            generation: generation.to_owned(),
            sha256: content_sha256,
        },
        root_mode: 0o700,
        entries,
        total_bytes: contents.len() as u64,
        model_calls: 0,
    };
    manifest.validate().unwrap();
    manifest
}

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
