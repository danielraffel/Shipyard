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
