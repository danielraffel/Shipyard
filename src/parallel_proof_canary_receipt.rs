//! Compact, immutable measurements for the default-off Pulp macOS canary.
//!
//! This module records evidence; it does not discover hosts, transfer bytes,
//! dispatch work, persist records, or satisfy merge readiness. Callers must
//! durably publish the returned receipt through the controller-owned record
//! store before treating it as retained evidence.

use serde::{Deserialize, Serialize};

use crate::parallel_proof::{ParallelProofContext, ParallelProofError, Sha256Digest};
use crate::parallel_proof_canary::{
    CanaryCacheGeneration, CanaryHostObservation, CanaryRoute, INITIAL_BUILDER, PULP_MAC_TARGET,
    PULP_REPOSITORY, PULP_REPOSITORY_ID, PulpMacCanaryDecision, canary_host_observations_digest,
    is_pulp_mac_canary_scope,
};

/// Current immutable measurement-receipt schema.
pub const PULP_MAC_CANARY_MEASUREMENT_SCHEMA: u32 = 2;
const MINIMUM_SAVINGS_MS: u64 = 120_000;
const MINIMUM_SAVINGS_PERCENT: u64 = 10;
const MAX_OVERHEAD_PERCENT: u64 = 15;
const MAX_CORRELATION_ID_BYTES: usize = 128;
const MAX_CACHE_MEASUREMENTS: usize = 256;

/// How the exact encoded artifact became available on the worker.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactDeliveryMode {
    /// Every encoded byte was transferred during this attempt.
    FullTransfer,
    /// A verified prefix survived an interruption and only the suffix moved.
    VerifiedPrefixResume,
    /// A complete digest-addressed object already existed and was reverified.
    ImmutableObjectReuse,
}

/// Whether one exact host-local cache generation was used by this canary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheUse {
    /// The build or shard consumed this exact generation.
    Hit,
    /// The exact generation was present but this execution did not consume it.
    PresentUnused,
}

/// Measured reuse of one exact host-local cache generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryCacheMeasurement {
    /// Immutable cache identity authenticated during admission.
    pub generation: CanaryCacheGeneration,
    /// Whether this execution consumed the generation.
    pub usage: CacheUse,
    /// Reserved untrusted diagnostic field; canonical receipts require zero.
    /// Actual transfer counters are recorded separately by the controller.
    pub claimed_bytes_avoided: u64,
}

/// Separately validated same-proof single-host timing control.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SingleHostControlReceipt {
    /// Control receipt schema.
    pub schema_version: u32,
    /// Complete proof-manifest digest.
    pub manifest_digest: Sha256Digest,
    /// Exact source repository database identity.
    pub repository_id: u64,
    /// Exact canonical source repository.
    pub repository: String,
    /// Exact source head.
    pub head_sha: String,
    /// Exact source tree.
    pub tree_sha: String,
    /// Exact encoded artifact digest.
    pub artifact_sha256: Sha256Digest,
    /// Authenticated single-host control worker.
    pub host_id: String,
    /// Exact authenticated control-host session generation.
    pub host_session_generation: u64,
    /// Controller time of the authenticated host observation.
    pub host_observed_at_ms: u64,
    /// Digest of the complete authenticated host observation.
    pub host_observation_digest: Sha256Digest,
    /// Submit-to-compact-receipt wall time for the single-host control.
    pub submit_to_receipt_ms: u64,
    /// Active worker time for the single-host control.
    pub worker_active_ms: u64,
    /// Model invocations; valid routine controls require zero.
    pub model_calls: u64,
}

impl SingleHostControlReceipt {
    /// Capture one pure control receipt from an authenticated M3 observation.
    pub fn capture(
        proof: ParallelProofContext<'_>,
        host: &CanaryHostObservation,
        submit_to_receipt_ms: u64,
        worker_active_ms: u64,
        model_calls: u64,
    ) -> Result<Self, ParallelProofError> {
        let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
        if !is_pulp_mac_canary_scope(proof)
            || host.host_id != INITIAL_BUILDER
            || host.route != CanaryRoute::SameHost
            || !host.online
            || host.session_generation == 0
            || host.observed_at_ms == 0
        {
            return Err(ParallelProofError::BindingMismatch(
                "single-host control identity",
            ));
        }
        let receipt = Self {
            schema_version: PULP_MAC_CANARY_MEASUREMENT_SCHEMA,
            manifest_digest: proof.manifest.digest(proof.inventory, proof.plan)?,
            repository_id: proof.manifest.source.repository_id,
            repository: proof.manifest.source.repository.clone(),
            head_sha: proof.manifest.source.head_sha.clone(),
            tree_sha: proof.manifest.source.tree_sha.clone(),
            artifact_sha256: proof.manifest.artifact.payload_sha256.clone(),
            host_id: host.host_id.clone(),
            host_session_generation: host.session_generation,
            host_observed_at_ms: host.observed_at_ms,
            host_observation_digest: canary_host_observation_digest(host)?,
            submit_to_receipt_ms,
            worker_active_ms,
            model_calls,
        };
        receipt.validate(proof, host)?;
        Ok(receipt)
    }

    /// Validate this control against the exact proof it claims to measure.
    pub fn validate(
        &self,
        proof: ParallelProofContext<'_>,
        host: &CanaryHostObservation,
    ) -> Result<(), ParallelProofError> {
        let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
        if self.schema_version != PULP_MAC_CANARY_MEASUREMENT_SCHEMA {
            return Err(ParallelProofError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        if !is_pulp_mac_canary_scope(proof)
            || self.repository_id != PULP_REPOSITORY_ID
            || self.repository != PULP_REPOSITORY
            || self.manifest_digest != proof.manifest.digest(proof.inventory, proof.plan)?
            || self.head_sha != proof.manifest.source.head_sha
            || self.tree_sha != proof.manifest.source.tree_sha
            || self.artifact_sha256 != proof.manifest.artifact.payload_sha256
            || self.host_id != INITIAL_BUILDER
            || self.host_id != host.host_id
            || self.host_session_generation != host.session_generation
            || self.host_observed_at_ms != host.observed_at_ms
            || self.host_observation_digest != canary_host_observation_digest(host)?
            || host.route != CanaryRoute::SameHost
            || !host.online
            || self.host_session_generation == 0
            || self.host_observed_at_ms == 0
            || self.submit_to_receipt_ms == 0
            || self.worker_active_ms == 0
            || self.worker_active_ms > self.submit_to_receipt_ms
            || self.model_calls != 0
        {
            return Err(ParallelProofError::BindingMismatch(
                "single-host control receipt",
            ));
        }
        Ok(())
    }

    /// Domain-separated digest of the exact validated control receipt.
    pub fn digest(
        &self,
        proof: ParallelProofContext<'_>,
        host: &CanaryHostObservation,
    ) -> Result<Sha256Digest, ParallelProofError> {
        self.validate(proof, host)?;
        canonical_digest("shipyard.pulp-mac-canary.single-host-control.v1", self)
    }
}

/// Controller observations needed to construct one immutable receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanaryMeasurementInput {
    /// Bounded, non-secret identifier joining controller logs and receipts.
    pub correlation_id: String,
    /// Artifact delivery behavior observed for this attempt.
    pub delivery_mode: ArtifactDeliveryMode,
    /// Complete encoded artifact size.
    pub artifact_bytes_total: u64,
    /// Previously verified encoded bytes reused by this attempt.
    pub artifact_bytes_reused: u64,
    /// Encoded bytes transferred by this attempt.
    pub artifact_bytes_transferred: u64,
    /// Exact verified prefix from which an interrupted transfer resumed.
    pub verified_resume_offset_bytes: u64,
    /// Setup work before transfer or execution.
    pub setup_ms: u64,
    /// Time spent transferring encoded bytes.
    pub transfer_ms: u64,
    /// Time spent verifying the artifact and extracted layout.
    pub verification_ms: u64,
    /// Controller dispatch and aggregation overhead.
    pub dispatch_ms: u64,
    /// Critical-path shard execution time, excluding transport overhead.
    pub shard_execution_ms: u64,
    /// Sum of active time across all workers.
    pub worker_active_ms: u64,
    /// Submit-to-compact-receipt wall clock for the distributed shadow canary.
    pub submit_to_receipt_ms: u64,
    /// Separately validated immutable same-proof single-host control.
    pub single_host_control: SingleHostControlReceipt,
    /// Exact cache-generation observations, sorted by cache name.
    pub caches: Vec<CanaryCacheMeasurement>,
    /// Must remain zero: routine measurement and monitoring invoke no model.
    pub model_calls: u64,
}

/// Exact-proof, exact-host compact receipt for one successful shadow canary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacCanaryMeasurementReceipt {
    /// Measurement receipt schema.
    pub schema_version: u32,
    /// Non-secret join key for controller logs and related receipts.
    pub correlation_id: String,
    /// Digest of the complete parallel-proof manifest, inventory, and plan.
    pub manifest_digest: Sha256Digest,
    /// Immutable GitHub repository database identifier.
    pub repository_id: u64,
    /// Canonical repository slug.
    pub repository: String,
    /// Exact Shipyard target admitted by the narrow canary policy.
    pub target: String,
    /// Exact proposal head executed by the canary.
    pub head_sha: String,
    /// Exact proposal tree executed by the canary.
    pub tree_sha: String,
    /// Digest of the encoded build artifact.
    pub artifact_sha256: Sha256Digest,
    /// Digest of the exact unpacked artifact layout.
    pub artifact_layout_sha256: Sha256Digest,
    /// Authenticated build host identifier.
    pub builder_host_id: String,
    /// Builder session generation fenced at admission.
    pub builder_session_generation: u64,
    /// Controller time of the authenticated builder observation.
    pub builder_observed_at_ms: u64,
    /// Authenticated secondary worker identifier.
    pub worker_host_id: String,
    /// Worker session generation fenced at admission.
    pub worker_session_generation: u64,
    /// Controller time of the authenticated worker observation.
    pub worker_observed_at_ms: u64,
    /// Digest of the complete admitted builder and worker observations.
    pub host_observations_digest: Sha256Digest,
    /// Authenticated transfer route used by the worker.
    pub route: CanaryRoute,
    /// How the worker obtained the artifact.
    pub delivery_mode: ArtifactDeliveryMode,
    /// Complete encoded artifact size.
    pub artifact_bytes_total: u64,
    /// Verified encoded bytes reused by this attempt.
    pub artifact_bytes_reused: u64,
    /// Encoded bytes transferred by this attempt.
    pub artifact_bytes_transferred: u64,
    /// Verified prefix offset used to resume an interrupted transfer.
    pub verified_resume_offset_bytes: u64,
    /// Pre-transfer setup duration.
    pub setup_ms: u64,
    /// Artifact transfer duration.
    pub transfer_ms: u64,
    /// Artifact and extracted-layout verification duration.
    pub verification_ms: u64,
    /// Controller dispatch and aggregation duration.
    pub dispatch_ms: u64,
    /// Critical-path shard execution duration.
    pub shard_execution_ms: u64,
    /// Sum of active time across every worker.
    pub worker_active_ms: u64,
    /// Submit-to-compact-receipt wall clock for this shadow canary.
    pub submit_to_receipt_ms: u64,
    /// Comparable single-host control wall clock.
    pub single_host_control_ms: u64,
    /// Exact proof-manifest digest named by the immutable control receipt.
    pub single_host_control_manifest_digest: Sha256Digest,
    /// Digest of the separately validated immutable control receipt.
    pub single_host_control_receipt_digest: Sha256Digest,
    /// Sorted exact host-local cache observations.
    pub caches: Vec<CanaryCacheMeasurement>,
    /// Model invocations; valid routine canary receipts require zero.
    pub model_calls: u64,
}

impl PulpMacCanaryMeasurementReceipt {
    /// Create an exact-proof receipt from an already eligible shadow decision.
    pub fn capture(
        proof: ParallelProofContext<'_>,
        decision: &PulpMacCanaryDecision,
        builder: &CanaryHostObservation,
        worker: &CanaryHostObservation,
        control_host: &CanaryHostObservation,
        input: CanaryMeasurementInput,
    ) -> Result<Self, ParallelProofError> {
        let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
        let manifest_digest = proof.manifest.digest(proof.inventory, proof.plan)?;
        let PulpMacCanaryDecision::Eligible {
            manifest_digest: admitted_manifest,
            builder_host_id,
            builder_session_generation,
            builder_observed_at_ms,
            worker_host_id,
            worker_session_generation,
            worker_observed_at_ms,
            host_observations_digest,
            ..
        } = decision
        else {
            return Err(ParallelProofError::InvalidField(
                "canary measurement admission",
            ));
        };
        if admitted_manifest != &manifest_digest {
            return Err(ParallelProofError::BindingMismatch(
                "canary measurement manifest",
            ));
        }
        if !is_pulp_mac_canary_scope(proof) {
            return Err(ParallelProofError::BindingMismatch(
                "Pulp macOS canary scope",
            ));
        }
        if builder.host_id != *builder_host_id
            || builder.session_generation != *builder_session_generation
            || builder.observed_at_ms != *builder_observed_at_ms
            || worker.host_id != *worker_host_id
            || worker.session_generation != *worker_session_generation
            || worker.observed_at_ms != *worker_observed_at_ms
            || canary_host_observations_digest(builder, worker)? != *host_observations_digest
            || builder.host_id == worker.host_id
            || builder.route != CanaryRoute::SameHost
            || worker.route != CanaryRoute::Lan
            || !builder.online
            || !worker.online
            || builder.session_generation == 0
            || worker.session_generation == 0
        {
            return Err(ParallelProofError::BindingMismatch(
                "canary measurement hosts",
            ));
        }
        input.single_host_control.validate(proof, control_host)?;
        let single_host_control_ms = input.single_host_control.submit_to_receipt_ms;
        let single_host_control_receipt_digest =
            input.single_host_control.digest(proof, control_host)?;
        validate_input(
            &input,
            proof.manifest.artifact.size_bytes,
            builder,
            worker,
            single_host_control_ms,
        )?;

        let receipt = Self {
            schema_version: PULP_MAC_CANARY_MEASUREMENT_SCHEMA,
            correlation_id: input.correlation_id,
            manifest_digest,
            repository_id: proof.manifest.source.repository_id,
            repository: proof.manifest.source.repository.clone(),
            target: PULP_MAC_TARGET.to_owned(),
            head_sha: proof.manifest.source.head_sha.clone(),
            tree_sha: proof.manifest.source.tree_sha.clone(),
            artifact_sha256: proof.manifest.artifact.payload_sha256.clone(),
            artifact_layout_sha256: proof.manifest.artifact.layout_sha256.clone(),
            builder_host_id: builder.host_id.clone(),
            builder_session_generation: builder.session_generation,
            builder_observed_at_ms: builder.observed_at_ms,
            worker_host_id: worker.host_id.clone(),
            worker_session_generation: worker.session_generation,
            worker_observed_at_ms: worker.observed_at_ms,
            host_observations_digest: host_observations_digest.clone(),
            route: worker.route,
            delivery_mode: input.delivery_mode,
            artifact_bytes_total: input.artifact_bytes_total,
            artifact_bytes_reused: input.artifact_bytes_reused,
            artifact_bytes_transferred: input.artifact_bytes_transferred,
            verified_resume_offset_bytes: input.verified_resume_offset_bytes,
            setup_ms: input.setup_ms,
            transfer_ms: input.transfer_ms,
            verification_ms: input.verification_ms,
            dispatch_ms: input.dispatch_ms,
            shard_execution_ms: input.shard_execution_ms,
            worker_active_ms: input.worker_active_ms,
            submit_to_receipt_ms: input.submit_to_receipt_ms,
            single_host_control_ms,
            single_host_control_manifest_digest: input.single_host_control.manifest_digest,
            single_host_control_receipt_digest,
            caches: input.caches,
            model_calls: input.model_calls,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    /// Validate internal invariants after deserialization.
    pub fn validate(&self) -> Result<(), ParallelProofError> {
        if self.schema_version != PULP_MAC_CANARY_MEASUREMENT_SCHEMA {
            return Err(ParallelProofError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        validate_correlation_id(&self.correlation_id)?;
        validate_delivery(
            self.delivery_mode,
            self.artifact_bytes_total,
            self.artifact_bytes_reused,
            self.artifact_bytes_transferred,
            self.verified_resume_offset_bytes,
        )?;
        validate_timings(
            self.setup_ms,
            self.transfer_ms,
            self.verification_ms,
            self.dispatch_ms,
            self.shard_execution_ms,
            self.worker_active_ms,
            self.submit_to_receipt_ms,
            self.single_host_control_ms,
            self.model_calls,
        )?;
        validate_cache_measurements(&self.caches, None)?;
        self.claimed_cache_bytes_avoided()?;
        if self.repository_id != PULP_REPOSITORY_ID
            || self.repository != PULP_REPOSITORY
            || self.target != PULP_MAC_TARGET
            || !valid_git_sha(&self.head_sha)
            || !valid_git_sha(&self.tree_sha)
            || self.head_sha.len() != self.tree_sha.len()
            || !valid_label(&self.builder_host_id)
            || !valid_label(&self.worker_host_id)
            || self.builder_host_id == self.worker_host_id
            || self.builder_session_generation == 0
            || self.worker_session_generation == 0
            || self.builder_observed_at_ms == 0
            || self.worker_observed_at_ms == 0
            || self.route != CanaryRoute::Lan
            || self.single_host_control_manifest_digest != self.manifest_digest
        {
            return Err(ParallelProofError::InvalidField(
                "canary measurement identity",
            ));
        }
        Ok(())
    }

    /// Domain-separated digest suitable for immutable publication.
    pub fn digest(&self) -> Result<Sha256Digest, ParallelProofError> {
        self.validate()?;
        canonical_digest("shipyard.pulp-mac-canary.measurement.v1", self)
    }

    /// `(hits, total)` for exact cache generations in this receipt.
    #[must_use]
    pub fn cache_hit_counts(&self) -> (u64, u64) {
        let hits = self
            .caches
            .iter()
            .filter(|cache| cache.usage == CacheUse::Hit)
            .count() as u64;
        (hits, self.caches.len() as u64)
    }

    /// Total untrusted diagnostic claim of bytes avoided by cache reuse.
    /// Canonical controller receipts require this to remain zero.
    pub fn claimed_cache_bytes_avoided(&self) -> Result<u64, ParallelProofError> {
        checked_sum(
            self.caches.iter().map(|cache| cache.claimed_bytes_avoided),
            "claimed cache bytes avoided",
        )
    }

    /// Whether measured latency and transfer overhead clear the promotion floor.
    #[must_use]
    pub fn meets_speed_gate(&self) -> bool {
        let savings = self
            .single_host_control_ms
            .saturating_sub(self.submit_to_receipt_ms);
        let percentage = u128::from(savings) * 100
            >= u128::from(self.single_host_control_ms) * u128::from(MINIMUM_SAVINGS_PERCENT);
        let overhead = u128::from(self.transport_overhead_ms());
        let shard = u128::from(self.shard_execution_ms);
        savings >= MINIMUM_SAVINGS_MS
            && percentage
            && overhead * 100 <= shard * u128::from(MAX_OVERHEAD_PERCENT)
    }

    /// Setup, transfer, verification, dispatch, and aggregation overhead.
    #[must_use]
    pub fn transport_overhead_ms(&self) -> u64 {
        self.setup_ms
            .saturating_add(self.transfer_ms)
            .saturating_add(self.verification_ms)
            .saturating_add(self.dispatch_ms)
    }

    /// This evidence is shadow-only and never satisfies merge readiness.
    #[must_use]
    pub const fn satisfies_merge_readiness(&self) -> bool {
        false
    }
}

fn canary_host_observation_digest(
    host: &CanaryHostObservation,
) -> Result<Sha256Digest, ParallelProofError> {
    canonical_digest("shipyard.pulp-mac-canary.host-observation.v1", host)
}

fn canonical_digest(
    domain: &str,
    value: &impl Serialize,
) -> Result<Sha256Digest, ParallelProofError> {
    let bytes = serde_json::to_vec(value)?;
    let mut canonical = Vec::with_capacity(16 + domain.len() + bytes.len());
    canonical.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    canonical.extend_from_slice(domain.as_bytes());
    canonical.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    canonical.extend_from_slice(&bytes);
    Ok(Sha256Digest::of_bytes(&canonical))
}

fn validate_input(
    input: &CanaryMeasurementInput,
    manifest_artifact_bytes: u64,
    builder: &CanaryHostObservation,
    worker: &CanaryHostObservation,
    single_host_control_ms: u64,
) -> Result<(), ParallelProofError> {
    validate_correlation_id(&input.correlation_id)?;
    if input.artifact_bytes_total != manifest_artifact_bytes {
        return Err(ParallelProofError::BindingMismatch(
            "canary measurement artifact size",
        ));
    }
    validate_delivery(
        input.delivery_mode,
        input.artifact_bytes_total,
        input.artifact_bytes_reused,
        input.artifact_bytes_transferred,
        input.verified_resume_offset_bytes,
    )?;
    validate_timings(
        input.setup_ms,
        input.transfer_ms,
        input.verification_ms,
        input.dispatch_ms,
        input.shard_execution_ms,
        input.worker_active_ms,
        input.submit_to_receipt_ms,
        single_host_control_ms,
        input.model_calls,
    )?;
    if builder.cache_generations != worker.cache_generations {
        return Err(ParallelProofError::BindingMismatch(
            "canary host cache generations",
        ));
    }
    validate_cache_measurements(&input.caches, Some(&worker.cache_generations))
}

pub(crate) fn validate_correlation_id(value: &str) -> Result<(), ParallelProofError> {
    if value.is_empty()
        || value.len() > MAX_CORRELATION_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':' | b'.'))
    {
        return Err(ParallelProofError::InvalidField(
            "canary measurement correlation id",
        ));
    }
    Ok(())
}

fn valid_git_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && value.trim() == value
        && !value.chars().any(char::is_control)
}

fn validate_delivery(
    mode: ArtifactDeliveryMode,
    total: u64,
    reused: u64,
    transferred: u64,
    resume_offset: u64,
) -> Result<(), ParallelProofError> {
    if total == 0 || reused.checked_add(transferred) != Some(total) {
        return Err(ParallelProofError::InvalidField(
            "canary measurement artifact bytes",
        ));
    }
    let valid = match mode {
        ArtifactDeliveryMode::FullTransfer => {
            reused == 0 && transferred == total && resume_offset == 0
        }
        ArtifactDeliveryMode::VerifiedPrefixResume => {
            reused > 0 && reused < total && transferred > 0 && resume_offset == reused
        }
        ArtifactDeliveryMode::ImmutableObjectReuse => {
            reused == total && transferred == 0 && resume_offset == total
        }
    };
    if !valid {
        return Err(ParallelProofError::InvalidField(
            "canary measurement delivery mode",
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_timings(
    setup_ms: u64,
    transfer_ms: u64,
    verification_ms: u64,
    dispatch_ms: u64,
    shard_execution_ms: u64,
    worker_active_ms: u64,
    submit_to_receipt_ms: u64,
    single_host_control_ms: u64,
    model_calls: u64,
) -> Result<(), ParallelProofError> {
    if shard_execution_ms == 0
        || worker_active_ms == 0
        || submit_to_receipt_ms == 0
        || single_host_control_ms == 0
        || model_calls != 0
    {
        return Err(ParallelProofError::InvalidField(
            "canary measurement timings",
        ));
    }
    let overhead = checked_sum(
        [setup_ms, transfer_ms, verification_ms, dispatch_ms],
        "canary measurement overhead",
    )?;
    if overhead
        .checked_add(shard_execution_ms)
        .is_none_or(|critical_path| critical_path > submit_to_receipt_ms)
        || worker_active_ms < shard_execution_ms
    {
        return Err(ParallelProofError::InvalidField(
            "canary measurement duration relationship",
        ));
    }
    Ok(())
}

fn validate_cache_measurements(
    caches: &[CanaryCacheMeasurement],
    expected: Option<&[CanaryCacheGeneration]>,
) -> Result<(), ParallelProofError> {
    if caches.is_empty() || caches.len() > MAX_CACHE_MEASUREMENTS {
        return Err(ParallelProofError::InvalidField(
            "canary cache measurements",
        ));
    }
    if !caches
        .windows(2)
        .all(|pair| pair[0].generation.name < pair[1].generation.name)
        || caches.iter().any(|cache| {
            !valid_label(&cache.generation.name)
                || !valid_label(&cache.generation.generation)
                || cache.claimed_bytes_avoided != 0
        })
    {
        return Err(ParallelProofError::InvalidField(
            "canary cache measurements",
        ));
    }
    if expected.is_some_and(|expected| {
        caches
            .iter()
            .map(|cache| &cache.generation)
            .ne(expected.iter())
    }) {
        return Err(ParallelProofError::BindingMismatch(
            "canary cache generations",
        ));
    }
    Ok(())
}

fn checked_sum(
    values: impl IntoIterator<Item = u64>,
    field: &'static str,
) -> Result<u64, ParallelProofError> {
    values.into_iter().try_fold(0_u64, |sum, value| {
        sum.checked_add(value)
            .ok_or(ParallelProofError::InvalidField(field))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel_proof::{
        ArtifactIdentity, ArtifactTrustClass, BuildIdentity, ExecutionBoundary,
        ParallelProofManifest, ProofSubject, ShardPlan, SourceIdentity, TestCase, TestInventory,
        TrustIdentity,
    };

    struct Fixture {
        inventory: TestInventory,
        plan: ShardPlan,
        manifest: ParallelProofManifest,
    }

    fn digest(label: &str) -> Sha256Digest {
        Sha256Digest::of_bytes(label.as_bytes())
    }

    fn fixture() -> Fixture {
        let inventory = TestInventory::new(vec![
            TestCase {
                id: "audio".to_owned(),
                dependencies: Vec::new(),
                fixture_setup: Vec::new(),
                fixture_required: Vec::new(),
                fixture_cleanup: Vec::new(),
                run_serial: false,
                resource_locks: Vec::new(),
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
            TestCase {
                id: "dsp".to_owned(),
                dependencies: Vec::new(),
                fixture_setup: Vec::new(),
                fixture_required: Vec::new(),
                fixture_cleanup: Vec::new(),
                run_serial: false,
                resource_locks: Vec::new(),
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
        ])
        .expect("inventory");
        let plan = ShardPlan::deterministic_balanced(&inventory, 2).expect("plan");
        let tree_sha = "b".repeat(64);
        let build = BuildIdentity {
            contract_sha256: digest("contract"),
            toolchain_sha256: digest("toolchain"),
            target_triple: "aarch64-apple-darwin".to_owned(),
            profile: "release".to_owned(),
        };
        let manifest = ParallelProofManifest::new(
            SourceIdentity {
                repository_id: 1_203_111_607,
                repository: "generous-corp/pulp".to_owned(),
                subject: ProofSubject::PullRequest { number: 1 },
                head_sha: "a".repeat(64),
                tree_sha: tree_sha.clone(),
            },
            build.clone(),
            ArtifactIdentity {
                source_tree_sha: tree_sha,
                build_contract_sha256: build.contract_sha256,
                payload_sha256: digest("artifact"),
                layout_sha256: digest("layout"),
                size_bytes: 1_000,
            },
            TrustIdentity {
                producer_identity_sha256: digest("producer"),
                image_sha256: digest("image"),
                policy_sha256: digest("policy"),
                artifact_class: ArtifactTrustClass::TrustedController,
                execution_boundary: ExecutionBoundary::TrustedHost,
                network_enabled: false,
                writable_host_mounts: false,
            },
            &inventory,
            &plan,
        )
        .expect("manifest");
        Fixture {
            inventory,
            plan,
            manifest,
        }
    }

    fn proof(fixture: &Fixture) -> ParallelProofContext<'_> {
        ParallelProofContext::new(&fixture.manifest, &fixture.inventory, &fixture.plan)
            .expect("proof")
    }

    fn cache() -> CanaryCacheGeneration {
        CanaryCacheGeneration {
            name: "skia".to_owned(),
            generation: "m124-arm64".to_owned(),
            sha256: digest("cache"),
        }
    }

    fn host(host_id: &str, route: CanaryRoute, generation: u64) -> CanaryHostObservation {
        CanaryHostObservation {
            host_id: host_id.to_owned(),
            online: true,
            observed_at_ms: 1,
            session_generation: generation,
            route,
            staging_root: format!("/var/lib/shipyard/{host_id}"),
            staging_class: crate::parallel_proof_canary::CanaryStagingClass::Persistent,
            free_bytes: 10_000,
            capabilities: vec!["macos-arm64".to_owned()],
            cache_generations: vec![cache()],
        }
    }

    fn decision(fixture: &Fixture) -> PulpMacCanaryDecision {
        let builder = host("m3", CanaryRoute::SameHost, 4);
        let worker = host("m1", CanaryRoute::Lan, 7);
        PulpMacCanaryDecision::Eligible {
            manifest_digest: fixture
                .manifest
                .digest(&fixture.inventory, &fixture.plan)
                .expect("digest"),
            builder_host_id: "m3".to_owned(),
            builder_session_generation: builder.session_generation,
            builder_observed_at_ms: builder.observed_at_ms,
            worker_host_id: "m1".to_owned(),
            worker_session_generation: worker.session_generation,
            worker_observed_at_ms: worker.observed_at_ms,
            host_observations_digest: canary_host_observations_digest(&builder, &worker)
                .expect("host observations"),
            shard_count: 2,
            fleet_exclusive_shards: 0,
            fleet_resource_locks: 0,
            predicted_savings_ms: 200_000,
            predicted_overhead_percent: 10,
        }
    }

    fn input(fixture: &Fixture) -> CanaryMeasurementInput {
        CanaryMeasurementInput {
            correlation_id: "pulp-pr-1-attempt-1".to_owned(),
            delivery_mode: ArtifactDeliveryMode::VerifiedPrefixResume,
            artifact_bytes_total: 1_000,
            artifact_bytes_reused: 400,
            artifact_bytes_transferred: 600,
            verified_resume_offset_bytes: 400,
            setup_ms: 10_000,
            transfer_ms: 30_000,
            verification_ms: 10_000,
            dispatch_ms: 20_000,
            shard_execution_ms: 600_000,
            worker_active_ms: 1_000_000,
            submit_to_receipt_ms: 700_000,
            single_host_control: SingleHostControlReceipt::capture(
                proof(fixture),
                &host("m3", CanaryRoute::SameHost, 3),
                1_000_000,
                900_000,
                0,
            )
            .expect("control receipt"),
            caches: vec![CanaryCacheMeasurement {
                generation: cache(),
                usage: CacheUse::Hit,
                claimed_bytes_avoided: 0,
            }],
            model_calls: 0,
        }
    }

    fn receipt(
        fixture: &Fixture,
        input: CanaryMeasurementInput,
    ) -> Result<PulpMacCanaryMeasurementReceipt, ParallelProofError> {
        PulpMacCanaryMeasurementReceipt::capture(
            proof(fixture),
            &decision(fixture),
            &host("m3", CanaryRoute::SameHost, 4),
            &host("m1", CanaryRoute::Lan, 7),
            &host("m3", CanaryRoute::SameHost, 3),
            input,
        )
    }

    #[test]
    fn exact_receipt_binds_proof_hosts_reuse_and_speed_metrics() {
        let fixture = fixture();
        let receipt = receipt(&fixture, input(&fixture)).expect("receipt");
        assert_eq!(receipt.cache_hit_counts(), (1, 1));
        assert_eq!(receipt.claimed_cache_bytes_avoided().expect("bytes"), 0);
        assert_eq!(receipt.transport_overhead_ms(), 70_000);
        assert!(receipt.meets_speed_gate());
        assert!(!receipt.satisfies_merge_readiness());
        assert_eq!(
            receipt.digest().expect("digest"),
            receipt.digest().expect("stable")
        );
    }

    #[test]
    fn artifact_byte_accounting_and_resume_mode_fail_closed() {
        let fixture = fixture();
        let mut measurement = input(&fixture);
        measurement.artifact_bytes_reused = 399;
        assert!(matches!(
            receipt(&fixture, measurement),
            Err(ParallelProofError::InvalidField(
                "canary measurement artifact bytes"
            ))
        ));

        let mut measurement = input(&fixture);
        measurement.delivery_mode = ArtifactDeliveryMode::FullTransfer;
        assert!(matches!(
            receipt(&fixture, measurement),
            Err(ParallelProofError::InvalidField(
                "canary measurement delivery mode"
            ))
        ));
    }

    #[test]
    fn host_session_route_and_manifest_drift_fail_closed() {
        let fixture = fixture();
        let mut wrong_decision = decision(&fixture);
        let PulpMacCanaryDecision::Eligible {
            manifest_digest, ..
        } = &mut wrong_decision
        else {
            unreachable!()
        };
        *manifest_digest = digest("other");
        assert!(matches!(
            PulpMacCanaryMeasurementReceipt::capture(
                proof(&fixture),
                &wrong_decision,
                &host("m3", CanaryRoute::SameHost, 4),
                &host("m1", CanaryRoute::Lan, 7),
                &host("m3", CanaryRoute::SameHost, 3),
                input(&fixture),
            ),
            Err(ParallelProofError::BindingMismatch(
                "canary measurement manifest"
            ))
        ));

        assert!(matches!(
            PulpMacCanaryMeasurementReceipt::capture(
                proof(&fixture),
                &decision(&fixture),
                &host("m3", CanaryRoute::SameHost, 4),
                &host("m1", CanaryRoute::Tailnet, 7),
                &host("m3", CanaryRoute::SameHost, 3),
                input(&fixture),
            ),
            Err(ParallelProofError::BindingMismatch(
                "canary measurement hosts"
            ))
        ));

        let mut reconnected = host("m1", CanaryRoute::Lan, 8);
        reconnected.observed_at_ms = 2;
        assert!(matches!(
            PulpMacCanaryMeasurementReceipt::capture(
                proof(&fixture),
                &decision(&fixture),
                &host("m3", CanaryRoute::SameHost, 4),
                &reconnected,
                &host("m3", CanaryRoute::SameHost, 3),
                input(&fixture),
            ),
            Err(ParallelProofError::BindingMismatch(
                "canary measurement hosts"
            ))
        ));
    }

    #[test]
    fn synthetic_eligible_decision_cannot_escape_fixed_pulp_scope() {
        let mut fixture = fixture();
        let measurement = input(&fixture);
        fixture.manifest.source.repository_id = 99;
        fixture.manifest.source.repository = "example/project".to_owned();
        assert!(matches!(
            receipt(&fixture, measurement),
            Err(ParallelProofError::BindingMismatch(
                "Pulp macOS canary scope"
            ))
        ));
    }

    #[test]
    fn cache_generation_and_no_model_contract_are_exact() {
        let fixture = fixture();
        let mut measurement = input(&fixture);
        measurement.caches[0].generation.generation = "stale".to_owned();
        assert!(matches!(
            receipt(&fixture, measurement),
            Err(ParallelProofError::BindingMismatch(
                "canary cache generations"
            ))
        ));

        let mut measurement = input(&fixture);
        measurement.model_calls = 1;
        assert!(matches!(
            receipt(&fixture, measurement),
            Err(ParallelProofError::InvalidField(
                "canary measurement timings"
            ))
        ));

        let mut untrusted_claim = receipt(&fixture, input(&fixture)).expect("receipt");
        untrusted_claim.caches[0].claimed_bytes_avoided = 1;
        assert!(matches!(
            untrusted_claim.validate(),
            Err(ParallelProofError::InvalidField(
                "canary cache measurements"
            ))
        ));
    }

    #[test]
    fn speed_control_must_name_an_immutable_same_proof_receipt() {
        let fixture = fixture();
        let mut measurement = input(&fixture);
        measurement.single_host_control.manifest_digest = digest("other proof");
        assert!(matches!(
            receipt(&fixture, measurement),
            Err(ParallelProofError::BindingMismatch(
                "single-host control receipt"
            ))
        ));
    }

    #[test]
    fn slower_or_overhead_heavy_measurement_is_retained_but_not_promoted() {
        let fixture = fixture();
        let mut measurement = input(&fixture);
        measurement.single_host_control.submit_to_receipt_ms = 750_000;
        measurement.single_host_control.worker_active_ms = 700_000;
        measurement.transfer_ms = 100_000;
        measurement.submit_to_receipt_ms = 800_000;
        let receipt = receipt(&fixture, measurement).expect("receipt");
        assert!(!receipt.meets_speed_gate());
        assert!(receipt.digest().is_ok());
    }

    #[test]
    fn critical_path_is_sequential_and_cache_byte_claims_do_not_promote() {
        let fixture = fixture();
        let mut inconsistent = input(&fixture);
        inconsistent.submit_to_receipt_ms = 669_999;
        assert!(matches!(
            receipt(&fixture, inconsistent),
            Err(ParallelProofError::InvalidField(
                "canary measurement duration relationship"
            ))
        ));

        let mut different_claim = input(&fixture);
        different_claim.caches[0].claimed_bytes_avoided = 1;
        assert!(matches!(
            receipt(&fixture, different_claim),
            Err(ParallelProofError::InvalidField(
                "canary cache measurements"
            ))
        ));
    }

    #[test]
    fn unknown_fields_and_tampering_are_rejected_after_deserialization() {
        let fixture = fixture();
        let receipt = receipt(&fixture, input(&fixture)).expect("receipt");
        let mut value = serde_json::to_value(&receipt).expect("json");
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<PulpMacCanaryMeasurementReceipt>(value).is_err());

        let mut tampered = receipt;
        tampered.worker_session_generation = 0;
        assert!(tampered.validate().is_err());
    }
}
