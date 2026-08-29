use super::{
    CacheGenerationManifest, CanaryRoute, REMOTE_M1_CACHE_PROTOCOL_SCHEMA, RemoteM1CacheAuthority,
    RemoteM1CacheAuthorityReceipt, RemoteM1CacheCarrierFailureClass, RemoteM1CacheRequest,
    RemoteM1CacheResponse, RemoteM1CacheTransportStats, Sha256Digest,
};

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
