fn validate_request(
    request: &PulpMacCacheProbeRequest,
    policy: &PulpMacCanaryPolicy,
) -> Result<(), CacheObserverError> {
    if !valid_correlation_id(&request.correlation_id)
        || policy.maximum_observation_age_ms == 0
        || !canary_policy_scope_valid(policy)
    {
        return Err(CacheObserverError::Invalid(
            "Pulp macOS cache probe request".to_owned(),
        ));
    }
    validate_specs(
        &request.builder,
        &policy.builder_host_id,
        &policy.required_cache_generations,
    )?;
    validate_specs(
        &request.worker,
        &policy.worker_host_id,
        &policy.required_cache_generations,
    )
}

fn validate_specs(
    specs: &[CacheGenerationProbeSpec],
    host_id: &str,
    required: &[CanaryCacheGeneration],
) -> Result<(), CacheObserverError> {
    let generations = specs
        .iter()
        .map(|spec| {
            spec.expected_manifest.validate()?;
            if spec.host_id != host_id {
                return Err(CacheObserverError::Invalid(
                    "cache probe host role".to_owned(),
                ));
            }
            Ok(spec.expected_manifest.generation.clone())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if generations != required || !generations_canonical(&generations) {
        return Err(CacheObserverError::Invalid(
            "cache probe required generations".to_owned(),
        ));
    }
    let host_digests = specs
        .iter()
        .map(|spec| &spec.host_observation_sha256)
        .collect::<BTreeSet<_>>();
    if (!specs.is_empty() && host_digests.len() != 1)
        || host_digests.iter().any(|digest| !valid_sha256(digest))
    {
        return Err(CacheObserverError::Invalid(
            "cache probe host observation binding".to_owned(),
        ));
    }
    Ok(())
}

fn validate_receipt_bindings(
    specs: &[CacheGenerationProbeSpec],
    receipts: &[CacheGenerationObservationReceipt],
    require_remote: bool,
) -> Result<(), CacheObserverError> {
    if specs.len() != receipts.len()
        || specs.iter().zip(receipts).any(|(spec, receipt)| {
            receipt.host_id != spec.host_id
                || receipt.host_observation_sha256 != spec.host_observation_sha256
                || receipt.manifest != spec.expected_manifest
                || receipt.cache_root != spec.root.to_str().unwrap_or_default()
                || require_remote != receipt.remote_authority.is_some()
        })
    {
        return Err(CacheObserverError::Invalid(
            "cache observation request binding".to_owned(),
        ));
    }
    Ok(())
}

fn validate_receipts_precede_assessment(
    receipts: &[CacheGenerationObservationReceipt],
    assessed_at_ms: u64,
) -> Result<(), CacheObserverError> {
    if receipts
        .iter()
        .any(|receipt| receipt.observed_at_ms > assessed_at_ms)
    {
        return Err(CacheObserverError::Invalid(
            "cache receipt postdates its evidence assessment".to_owned(),
        ));
    }
    Ok(())
}

fn policy_at(
    policy: &PulpMacCanaryPolicy,
    assessed_at_ms: u64,
) -> Result<PulpMacCanaryPolicy, CacheObserverError> {
    if assessed_at_ms == 0 || assessed_at_ms < policy.assessed_at_ms {
        return Err(CacheObserverError::Invalid(
            "cache controller assessment clock regressed".to_owned(),
        ));
    }
    let mut current = policy.clone();
    current.assessed_at_ms = assessed_at_ms;
    Ok(current)
}

fn validate_role_receipts(
    receipts: &[CacheGenerationObservationReceipt],
    host_id: &str,
    required: &[CanaryCacheGeneration],
    freshness: Option<(u64, u64)>,
    require_remote: bool,
) -> Result<(), CacheObserverError> {
    let generations = receipts
        .iter()
        .map(|receipt| {
            receipt.validate()?;
            let stale = freshness.is_some_and(|(assessed_at_ms, maximum_age_ms)| {
                assessed_at_ms == 0
                    || maximum_age_ms == 0
                    || receipt.observed_at_ms > assessed_at_ms
                    || assessed_at_ms.saturating_sub(receipt.observed_at_ms) > maximum_age_ms
            });
            if receipt.host_id != host_id || stale {
                return Err(CacheObserverError::Invalid(
                    "cache observation host or freshness fence".to_owned(),
                ));
            }
            if require_remote != receipt.remote_authority.is_some() {
                return Err(CacheObserverError::Invalid(
                    "cache observation transport authority".to_owned(),
                ));
            }
            Ok(receipt.manifest.generation.clone())
        })
        .collect::<Result<Vec<_>, _>>()?;
    if generations != required || !generations_canonical(&generations) {
        return Err(CacheObserverError::Invalid(
            "cache observation generation inventory".to_owned(),
        ));
    }
    Ok(())
}

fn role_host_digest(
    receipts: &[CacheGenerationObservationReceipt],
    required: &[CanaryCacheGeneration],
) -> Result<Option<Sha256Digest>, CacheObserverError> {
    if required.is_empty() {
        return Ok(None);
    }
    let digests = receipts
        .iter()
        .map(|receipt| receipt.host_observation_sha256.clone())
        .collect::<BTreeSet<_>>();
    if digests.len() != 1 {
        return Err(CacheObserverError::Invalid(
            "cache observation host digest inventory".to_owned(),
        ));
    }
    Ok(digests.into_iter().next())
}

fn generations_canonical(generations: &[CanaryCacheGeneration]) -> bool {
    generations
        .windows(2)
        .all(|pair| pair[0].name < pair[1].name)
        && generations.iter().all(|generation| {
            valid_label(&generation.name)
                && valid_label(&generation.generation)
                && valid_sha256(&generation.sha256)
        })
}

fn cache_content_digest(
    root_mode: u32,
    entries: &[CacheGenerationEntry],
) -> Result<Sha256Digest, CacheObserverError> {
    domain_digest(
        "shipyard.cache-generation.contents.v1",
        &(root_mode, entries),
    )
}

fn domain_digest<T: Serialize + ?Sized>(
    domain: &str,
    value: &T,
) -> Result<Sha256Digest, CacheObserverError> {
    let payload = serde_json::to_vec(value)?;
    let mut bytes = Vec::with_capacity(16 + domain.len() + payload.len());
    bytes.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    bytes.extend_from_slice(domain.as_bytes());
    bytes.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    bytes.extend_from_slice(&payload);
    Ok(Sha256Digest::of_bytes(&bytes))
}

fn parse_sha256(value: &str) -> Result<Sha256Digest, CacheObserverError> {
    Sha256Digest::parse(value.to_owned())
        .map_err(|_| CacheObserverError::Invalid("cache entry SHA-256".to_owned()))
}

fn valid_sha256(value: &Sha256Digest) -> bool {
    value.as_str().len() == 64
        && value
            .as_str()
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

fn valid_correlation_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_CORRELATION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':'))
}

fn valid_relative_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn safe_absolute_cache_root(path: &Path) -> bool {
    let Some(value) = path.to_str() else {
        return false;
    };
    // Cache receipts may describe a remote macOS host even when the
    // controller is compiled on Windows. `Path::is_absolute` applies the
    // controller's native path grammar, so also recognize a lexically
    // absolute POSIX root. The remaining checks keep traversal, temporary
    // roots, and malformed components fail closed.
    (path.is_absolute() || value.starts_with('/'))
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
    u64::try_from(millis)
        .map_err(|_| CacheObserverError::Invalid("controller clock overflow".to_owned()))
}

fn milliseconds_ceil(duration: Duration) -> Result<u64, CacheObserverError> {
    let millis = duration.as_millis();
    let millis = if duration.subsec_nanos().is_multiple_of(1_000_000) {
        millis
    } else {
        millis.checked_add(1).ok_or_else(|| {
            CacheObserverError::Invalid("cache observation duration overflow".to_owned())
        })?
    };
    u64::try_from(millis)
        .map_err(|_| CacheObserverError::Invalid("cache observation duration overflow".to_owned()))
}
