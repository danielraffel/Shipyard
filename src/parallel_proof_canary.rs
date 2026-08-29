//! Pure, default-off admission policy for the first Pulp macOS sharding canary.
//!
//! This module does not discover hosts, transfer artifacts, dispatch shards, or
//! publish evidence. It only decides whether controller-owned observations are
//! sufficient to attempt a shadow canary using M3 as builder and M1 as the
//! secondary worker. Re-evaluation after an offline observation is the recovery
//! mechanism; no partially dispatched state is created here.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::parallel_proof::{
    MAX_CAPABILITIES, ParallelProofContext, ParallelProofError, ResourceLockScope, Sha256Digest,
    ShardExecutionMode,
};

pub(crate) const PULP_REPOSITORY: &str = "generous-corp/pulp";
pub(crate) const PULP_REPOSITORY_ID: u64 = 1_203_111_607;
pub(crate) const PULP_MAC_TARGET: &str = "mac";
pub(crate) const INITIAL_BUILDER: &str = "m3";
pub(crate) const INITIAL_WORKER: &str = "m1";
const MINIMUM_SAVINGS_MS: u64 = 120_000;
const MINIMUM_SAVINGS_PERCENT: u64 = 10;
const MAX_OVERHEAD_PERCENT: u64 = 15;

/// Network path authenticated by the controller for one host observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryRoute {
    /// Artifact remains on the build host.
    SameHost,
    /// Direct local-area-network route.
    Lan,
    /// Overlay-network route. Excluded from the first canary.
    Tailnet,
    /// No usable route was observed.
    Unavailable,
}

/// Controller-authenticated storage classification for a declared staging root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryStagingClass {
    /// Durable host storage suitable for resumable artifact staging.
    Persistent,
    /// OS-managed or otherwise ephemeral scratch storage.
    Temporary,
}

/// Exact external-cache generation already present on a host.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryCacheGeneration {
    /// Stable cache family name, such as `skia`.
    pub name: String,
    /// Immutable generation selected by repository policy.
    pub generation: String,
    /// Digest of the generation's canonical contents.
    pub sha256: Sha256Digest,
}

/// Controller-owned, point-in-time eligibility observation for one host.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryHostObservation {
    /// Stable configured fleet host identifier.
    pub host_id: String,
    /// Whether the authenticated host session is currently reachable.
    pub online: bool,
    /// Controller clock time at which this observation was captured.
    pub observed_at_ms: u64,
    /// Nonzero authenticated session generation used to fence reconnects.
    pub session_generation: u64,
    /// Route observed from the artifact producer to this host.
    pub route: CanaryRoute,
    /// Absolute staging root declared by host configuration, never inferred.
    pub staging_root: String,
    /// Authenticated storage class for the declared staging root.
    pub staging_class: CanaryStagingClass,
    /// Free bytes observed on the filesystem containing the staging root.
    pub free_bytes: u64,
    /// Sorted, unique authenticated capabilities.
    pub capabilities: Vec<String>,
    /// Sorted, unique exact cache generations.
    pub cache_generations: Vec<CanaryCacheGeneration>,
}

/// Controller-owned wall-time estimate. It is evidence input, not a scheduler.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanaryTimingEstimate {
    /// Exact proof manifest for which these estimates were produced.
    pub manifest_digest: Sha256Digest,
    /// Independently observed Shipyard target for this timing evidence.
    pub target: String,
    /// Predicted authoritative-suite wall time on the fastest single host.
    pub single_host_ms: u64,
    /// Predicted critical-path duration of all distributed shard work.
    pub distributed_shard_ms: u64,
    /// Predicted artifact transfer plus controller dispatch overhead.
    pub transfer_and_dispatch_ms: u64,
}

/// Narrow rollout policy for the initial Pulp macOS shadow canary.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PulpMacCanaryPolicy {
    /// Explicit opt-in. The default remains false.
    pub enabled: bool,
    /// Exact canonical repository slug admitted by this policy.
    pub repository: String,
    /// Exact Shipyard target admitted by this policy.
    pub target: String,
    /// Host that produced the immutable build artifact.
    pub builder_host_id: String,
    /// Secondary host that receives the artifact over the LAN.
    pub worker_host_id: String,
    /// Controller clock time at which this assessment occurs.
    pub assessed_at_ms: u64,
    /// Maximum accepted age for any host observation.
    pub maximum_observation_age_ms: u64,
    /// Free-space reserve retained after staging the artifact.
    pub minimum_free_bytes: u64,
    /// Sorted exact generations required on both hosts. Large shared caches are
    /// never transferred with the build artifact.
    pub required_cache_generations: Vec<CanaryCacheGeneration>,
}

impl Default for PulpMacCanaryPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            repository: PULP_REPOSITORY.to_owned(),
            target: PULP_MAC_TARGET.to_owned(),
            builder_host_id: INITIAL_BUILDER.to_owned(),
            worker_host_id: INITIAL_WORKER.to_owned(),
            assessed_at_ms: 0,
            maximum_observation_age_ms: 60_000,
            minimum_free_bytes: 0,
            required_cache_generations: Vec::new(),
        }
    }
}

/// Stable fail-closed reason suitable for shadow-trial telemetry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CanaryIneligibleReason {
    /// Repository, target, or target triple is outside the initial canary.
    WrongScope,
    /// A host pair other than M3 builder and M1 worker was requested.
    InitialHostPairRequired,
    /// One host is absent or ambiguously duplicated.
    HostMissing,
    /// One required host is offline.
    HostOffline,
    /// A host observation is expired or from the future.
    StaleObservation,
    /// A host cannot fence reconnects with a nonzero session generation.
    SessionGenerationMissing,
    /// The builder is not same-host or the worker is not directly on the LAN.
    RouteIneligible,
    /// A host did not declare a safe absolute staging root.
    StagingRootInvalid,
    /// Staging the artifact would violate the configured free-space reserve.
    InsufficientSpace,
    /// Exact host-local cache identities differ from policy.
    CacheGenerationMismatch,
    /// A host cannot execute every topology-bound shard.
    CapabilityMismatch,
    /// The bound plan has fewer than two parallelizable shards.
    NoParallelWork,
    /// Timing evidence belongs to another exact proof.
    TimingIdentityMismatch,
    /// Predicted end-to-end wall-time savings are immaterial.
    BenefitTooSmall,
    /// Transfer and dispatch consume too much of shard execution time.
    TransferOverheadTooHigh,
}

/// Pure admission result. Neither variant has merge authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PulpMacCanaryDecision {
    /// Policy has not explicitly opted in.
    Disabled,
    /// The observation failed one or more stable fail-closed checks.
    Ineligible {
        /// Sorted, deduplicated reasons for rejecting this observation.
        reasons: Vec<CanaryIneligibleReason>,
    },
    /// The exact observation is suitable for one shadow-only canary attempt.
    Eligible {
        /// Digest binding the exact source, build, artifact, inventory, and plan.
        manifest_digest: Sha256Digest,
        /// Exact build host selected by the narrow policy.
        builder_host_id: String,
        /// Exact authenticated builder session admitted by this decision.
        builder_session_generation: u64,
        /// Controller time of the admitted builder observation.
        builder_observed_at_ms: u64,
        /// Exact secondary worker selected by the narrow policy.
        worker_host_id: String,
        /// Exact authenticated worker session admitted by this decision.
        worker_session_generation: u64,
        /// Controller time of the admitted worker observation.
        worker_observed_at_ms: u64,
        /// Digest of both complete admitted host observations in role order.
        host_observations_digest: Sha256Digest,
        /// Number of exhaustive, disjoint shards in the bound plan.
        shard_count: u32,
        /// Fleet-exclusive shards retained from `RUN_SERIAL` declarations.
        fleet_exclusive_shards: u32,
        /// Unique fleet-wide resource-lock claims retained by the inventory.
        fleet_resource_locks: u32,
        /// Predicted end-to-end wall-time reduction.
        predicted_savings_ms: u64,
        /// Transfer and dispatch overhead as a percentage of shard work.
        predicted_overhead_percent: u64,
    },
}

impl PulpMacCanaryDecision {
    /// This schema is permanently shadow-only.
    #[must_use]
    pub const fn satisfies_merge_readiness(&self) -> bool {
        false
    }
}

/// Assess whether the exact proof may enter the initial shadow canary.
pub fn assess_pulp_mac_canary(
    proof: ParallelProofContext<'_>,
    policy: &PulpMacCanaryPolicy,
    hosts: &[CanaryHostObservation],
    timing: &CanaryTimingEstimate,
) -> Result<PulpMacCanaryDecision, ParallelProofError> {
    // Validate manifest, exhaustive membership, dependencies, fixtures,
    // RUN_SERIAL isolation, and resource-lock topology before policy checks.
    let proof = ParallelProofContext::new(proof.manifest, proof.inventory, proof.plan)?;
    if !policy.enabled {
        return Ok(PulpMacCanaryDecision::Disabled);
    }

    let manifest_digest = proof.manifest.digest(proof.inventory, proof.plan)?;
    let mut reasons = BTreeSet::new();
    if policy.repository != PULP_REPOSITORY
        || policy.target != PULP_MAC_TARGET
        || timing.target != PULP_MAC_TARGET
        || timing.target != policy.target
        || proof.manifest.source.repository != PULP_REPOSITORY
        || proof.manifest.source.repository_id != PULP_REPOSITORY_ID
        || proof.manifest.build.target_triple != "aarch64-apple-darwin"
    {
        reasons.insert(CanaryIneligibleReason::WrongScope);
    }
    if policy.builder_host_id != INITIAL_BUILDER || policy.worker_host_id != INITIAL_WORKER {
        reasons.insert(CanaryIneligibleReason::InitialHostPairRequired);
    }
    if proof.plan.shards.len() < 2
        || proof
            .plan
            .shards
            .iter()
            .filter(|shard| shard.execution_mode == ShardExecutionMode::Parallel)
            .count()
            < 2
    {
        reasons.insert(CanaryIneligibleReason::NoParallelWork);
    }
    if timing.manifest_digest != manifest_digest {
        reasons.insert(CanaryIneligibleReason::TimingIdentityMismatch);
    }

    let builder = unique_host(hosts, &policy.builder_host_id);
    let worker = unique_host(hosts, &policy.worker_host_id);
    match (builder, worker) {
        (Some(builder), Some(worker)) => {
            assess_host(policy, proof, builder, CanaryRoute::SameHost, &mut reasons);
            assess_host(policy, proof, worker, CanaryRoute::Lan, &mut reasons);
        }
        _ => {
            reasons.insert(CanaryIneligibleReason::HostMissing);
        }
    }

    let (savings, overhead_percent) = assess_timing(timing, &mut reasons);

    if !reasons.is_empty() {
        return Ok(PulpMacCanaryDecision::Ineligible {
            reasons: reasons.into_iter().collect(),
        });
    }

    let fleet_exclusive_shards = proof
        .plan
        .shards
        .iter()
        .filter(|shard| shard.execution_mode == ShardExecutionMode::FleetExclusive)
        .count();
    let fleet_resource_locks = proof
        .inventory
        .tests
        .iter()
        .flat_map(|test| &test.resource_locks)
        .filter(|lock| lock.scope == ResourceLockScope::Fleet)
        .map(|lock| lock.name.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let (Some(builder), Some(worker)) = (builder, worker) else {
        return Err(ParallelProofError::InvalidField(
            "eligible canary host observations",
        ));
    };
    Ok(PulpMacCanaryDecision::Eligible {
        manifest_digest,
        builder_host_id: policy.builder_host_id.clone(),
        builder_session_generation: builder.session_generation,
        builder_observed_at_ms: builder.observed_at_ms,
        worker_host_id: policy.worker_host_id.clone(),
        worker_session_generation: worker.session_generation,
        worker_observed_at_ms: worker.observed_at_ms,
        host_observations_digest: canary_host_observations_digest(builder, worker)?,
        shard_count: u32::try_from(proof.plan.shards.len())
            .map_err(|_| ParallelProofError::InvalidField("canary shard count"))?,
        fleet_exclusive_shards: u32::try_from(fleet_exclusive_shards)
            .map_err(|_| ParallelProofError::InvalidField("fleet exclusive shard count"))?,
        fleet_resource_locks: u32::try_from(fleet_resource_locks)
            .map_err(|_| ParallelProofError::InvalidField("fleet resource lock count"))?,
        predicted_savings_ms: savings,
        predicted_overhead_percent: overhead_percent,
    })
}

pub(crate) fn is_pulp_mac_canary_scope(proof: ParallelProofContext<'_>) -> bool {
    proof.manifest.source.repository_id == PULP_REPOSITORY_ID
        && proof.manifest.source.repository == PULP_REPOSITORY
        && proof.manifest.build.target_triple == "aarch64-apple-darwin"
}

pub(crate) fn canary_host_observations_digest(
    builder: &CanaryHostObservation,
    worker: &CanaryHostObservation,
) -> Result<Sha256Digest, ParallelProofError> {
    let bytes = serde_json::to_vec(&(builder, worker))?;
    let domain = b"shipyard.pulp-mac-canary.host-observations.v1";
    let mut canonical = Vec::with_capacity(16 + domain.len() + bytes.len());
    canonical.extend_from_slice(&(domain.len() as u64).to_be_bytes());
    canonical.extend_from_slice(domain);
    canonical.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    canonical.extend_from_slice(&bytes);
    Ok(Sha256Digest::of_bytes(&canonical))
}

fn assess_timing(
    timing: &CanaryTimingEstimate,
    reasons: &mut BTreeSet<CanaryIneligibleReason>,
) -> (u64, u64) {
    let savings = timing.single_host_ms.saturating_sub(
        timing
            .distributed_shard_ms
            .saturating_add(timing.transfer_and_dispatch_ms),
    );
    let meets_percentage_savings = u128::from(savings) * 100
        >= u128::from(timing.single_host_ms) * u128::from(MINIMUM_SAVINGS_PERCENT);
    if timing.single_host_ms == 0
        || timing.distributed_shard_ms == 0
        || savings < MINIMUM_SAVINGS_MS
        || !meets_percentage_savings
    {
        reasons.insert(CanaryIneligibleReason::BenefitTooSmall);
    }

    let overhead_numerator = u128::from(timing.transfer_and_dispatch_ms) * 100;
    let overhead_denominator = u128::from(timing.distributed_shard_ms);
    let overhead_percent = overhead_numerator
        .checked_div(overhead_denominator)
        .and_then(|percent| u64::try_from(percent).ok())
        .unwrap_or(u64::MAX);
    if overhead_denominator == 0
        || overhead_numerator > overhead_denominator * u128::from(MAX_OVERHEAD_PERCENT)
    {
        reasons.insert(CanaryIneligibleReason::TransferOverheadTooHigh);
    }
    (savings, overhead_percent)
}

fn unique_host<'a>(
    hosts: &'a [CanaryHostObservation],
    host_id: &str,
) -> Option<&'a CanaryHostObservation> {
    let mut matching = hosts.iter().filter(|host| host.host_id == host_id);
    let host = matching.next()?;
    matching.next().is_none().then_some(host)
}

fn assess_host(
    policy: &PulpMacCanaryPolicy,
    proof: ParallelProofContext<'_>,
    host: &CanaryHostObservation,
    required_route: CanaryRoute,
    reasons: &mut BTreeSet<CanaryIneligibleReason>,
) {
    if !host.online {
        reasons.insert(CanaryIneligibleReason::HostOffline);
    }
    if host.session_generation == 0 {
        reasons.insert(CanaryIneligibleReason::SessionGenerationMissing);
    }
    if policy.assessed_at_ms == 0
        || policy.maximum_observation_age_ms == 0
        || host.observed_at_ms > policy.assessed_at_ms
        || policy.assessed_at_ms.saturating_sub(host.observed_at_ms)
            > policy.maximum_observation_age_ms
    {
        reasons.insert(CanaryIneligibleReason::StaleObservation);
    }
    if host.route != required_route {
        reasons.insert(CanaryIneligibleReason::RouteIneligible);
    }
    if !safe_persistent_staging_root(host) {
        reasons.insert(CanaryIneligibleReason::StagingRootInvalid);
    }
    let required_free = policy
        .minimum_free_bytes
        .checked_add(proof.manifest.artifact.size_bytes);
    if required_free.is_none_or(|required| host.free_bytes < required) {
        reasons.insert(CanaryIneligibleReason::InsufficientSpace);
    }
    if !cache_generations_canonical(&host.cache_generations)
        || !cache_generations_canonical(&policy.required_cache_generations)
        || host.cache_generations != policy.required_cache_generations
    {
        reasons.insert(CanaryIneligibleReason::CacheGenerationMismatch);
    }
    let capabilities = host
        .capabilities
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if host.capabilities.len() > MAX_CAPABILITIES
        || !strictly_sorted_unique(&host.capabilities)
        || !capabilities.contains("macos-arm64")
        || proof.inventory.tests.iter().any(|test| {
            !test
                .required_capabilities
                .iter()
                .all(|required| capabilities.contains(required.as_str()))
        })
    {
        reasons.insert(CanaryIneligibleReason::CapabilityMismatch);
    }
}

fn strictly_sorted_unique<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

fn safe_persistent_staging_root(host: &CanaryHostObservation) -> bool {
    host.staging_class == CanaryStagingClass::Persistent
        && canonical_absolute_macos_path(&host.staging_root)
        && [
            "/tmp",
            "/private/tmp",
            "/var/tmp",
            "/private/var/tmp",
            "/var/folders",
            "/private/var/folders",
        ]
        .iter()
        .all(|temporary| !macos_path_is_within(&host.staging_root, temporary))
}

fn canonical_absolute_macos_path(value: &str) -> bool {
    value.starts_with('/')
        && value != "/"
        && !value.ends_with('/')
        && !value.chars().any(char::is_control)
        && value
            .split('/')
            .skip(1)
            .all(|component| !component.is_empty() && component != "." && component != "..")
}

fn macos_path_is_within(path: &str, ancestor: &str) -> bool {
    path == ancestor
        || path
            .strip_prefix(ancestor)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn cache_generations_canonical(values: &[CanaryCacheGeneration]) -> bool {
    !values.is_empty()
        && strictly_sorted_unique(values)
        && values.iter().all(|value| {
            !value.name.is_empty()
                && !value.generation.is_empty()
                && !value.name.chars().any(char::is_control)
                && !value.generation.chars().any(char::is_control)
        })
        && values.windows(2).all(|pair| pair[0].name != pair[1].name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parallel_proof::{
        ArtifactIdentity, ArtifactTrustClass, BuildIdentity, ExecutionBoundary,
        ParallelProofManifest, ProofSubject, ResourceLock, ShardPlan, SourceIdentity, TestCase,
        TestInventory, TrustIdentity,
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
                resource_locks: vec![ResourceLock {
                    name: "coreaudio".to_owned(),
                    scope: ResourceLockScope::Fleet,
                }],
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
            TestCase {
                id: "release".to_owned(),
                dependencies: Vec::new(),
                fixture_setup: Vec::new(),
                fixture_required: Vec::new(),
                fixture_cleanup: Vec::new(),
                run_serial: true,
                resource_locks: Vec::new(),
                required_capabilities: vec!["macos-arm64".to_owned()],
            },
        ])
        .expect("inventory");
        let plan = ShardPlan::deterministic_balanced(&inventory, 3).expect("plan");
        let tree_sha = "b".repeat(64);
        let build = BuildIdentity {
            contract_sha256: digest("contract"),
            toolchain_sha256: digest("toolchain"),
            target_triple: "aarch64-apple-darwin".to_owned(),
            profile: "release".to_owned(),
        };
        let artifact = ArtifactIdentity {
            source_tree_sha: tree_sha.clone(),
            build_contract_sha256: build.contract_sha256.clone(),
            payload_sha256: digest("artifact"),
            layout_sha256: digest("layout"),
            size_bytes: 1024,
        };
        let manifest = ParallelProofManifest::new(
            SourceIdentity {
                repository_id: PULP_REPOSITORY_ID,
                repository: PULP_REPOSITORY.to_owned(),
                subject: ProofSubject::PullRequest { number: 1 },
                head_sha: "a".repeat(64),
                tree_sha,
            },
            build,
            artifact,
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

    fn cache() -> CanaryCacheGeneration {
        CanaryCacheGeneration {
            name: "skia".to_owned(),
            generation: "m124-arm64".to_owned(),
            sha256: digest("skia-cache"),
        }
    }

    fn policy() -> PulpMacCanaryPolicy {
        PulpMacCanaryPolicy {
            enabled: true,
            assessed_at_ms: 10_000,
            maximum_observation_age_ms: 1_000,
            minimum_free_bytes: 100,
            required_cache_generations: vec![cache()],
            ..PulpMacCanaryPolicy::default()
        }
    }

    fn host(host_id: &str, route: CanaryRoute) -> CanaryHostObservation {
        CanaryHostObservation {
            host_id: host_id.to_owned(),
            online: true,
            observed_at_ms: 9_500,
            session_generation: 1,
            route,
            staging_root: format!("/var/lib/shipyard/{host_id}"),
            staging_class: CanaryStagingClass::Persistent,
            free_bytes: 2_000,
            capabilities: vec!["macos-arm64".to_owned()],
            cache_generations: vec![cache()],
        }
    }

    fn proof(fixture: &Fixture) -> ParallelProofContext<'_> {
        ParallelProofContext::new(&fixture.manifest, &fixture.inventory, &fixture.plan)
            .expect("proof")
    }

    fn timing(fixture: &Fixture, transfer_and_dispatch_ms: u64) -> CanaryTimingEstimate {
        CanaryTimingEstimate {
            manifest_digest: fixture
                .manifest
                .digest(&fixture.inventory, &fixture.plan)
                .expect("manifest digest"),
            target: PULP_MAC_TARGET.to_owned(),
            single_host_ms: 3_300_000,
            distributed_shard_ms: 1_500_000,
            transfer_and_dispatch_ms,
        }
    }

    #[test]
    fn default_policy_is_disabled_and_never_merge_authoritative() {
        let fixture = fixture();
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &PulpMacCanaryPolicy::default(),
            &[],
            &CanaryTimingEstimate {
                manifest_digest: digest("unused-disabled-estimate"),
                target: PULP_MAC_TARGET.to_owned(),
                single_host_ms: 0,
                distributed_shard_ms: 0,
                transfer_and_dispatch_ms: 0,
            },
        )
        .expect("decision");
        assert_eq!(decision, PulpMacCanaryDecision::Disabled);
        assert!(!decision.satisfies_merge_readiness());
    }

    #[test]
    fn exact_m3_m1_lan_pair_with_material_benefit_is_eligible() {
        let fixture = fixture();
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[
                host(INITIAL_BUILDER, CanaryRoute::SameHost),
                host(INITIAL_WORKER, CanaryRoute::Lan),
            ],
            &timing(&fixture, 120_000),
        )
        .expect("decision");
        let PulpMacCanaryDecision::Eligible {
            fleet_exclusive_shards,
            fleet_resource_locks,
            predicted_savings_ms,
            ..
        } = decision
        else {
            panic!("expected eligible");
        };
        assert_eq!(fleet_exclusive_shards, 1);
        assert_eq!(fleet_resource_locks, 1);
        assert_eq!(predicted_savings_ms, 1_680_000);
    }

    #[test]
    fn roaming_or_offline_worker_fails_closed_until_fresh_lan_observation() {
        let fixture = fixture();
        let mut worker = host(INITIAL_WORKER, CanaryRoute::Tailnet);
        worker.online = false;
        worker.observed_at_ms = 1;
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[host(INITIAL_BUILDER, CanaryRoute::SameHost), worker],
            &timing(&fixture, 120_000),
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::HostOffline));
        assert!(reasons.contains(&CanaryIneligibleReason::StaleObservation));
        assert!(reasons.contains(&CanaryIneligibleReason::RouteIneligible));
    }

    #[test]
    fn cache_drift_or_excess_transfer_cost_fails_closed() {
        let fixture = fixture();
        let mut worker = host(INITIAL_WORKER, CanaryRoute::Lan);
        worker.cache_generations[0].generation = "other".to_owned();
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[host(INITIAL_BUILDER, CanaryRoute::SameHost), worker],
            &timing(&fixture, 300_000),
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::CacheGenerationMismatch));
        assert!(reasons.contains(&CanaryIneligibleReason::TransferOverheadTooHigh));
    }

    #[test]
    fn timing_evidence_cannot_be_reused_for_another_exact_proof() {
        let fixture = fixture();
        let mut estimate = timing(&fixture, 120_000);
        estimate.manifest_digest = digest("another-proof");
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[
                host(INITIAL_BUILDER, CanaryRoute::SameHost),
                host(INITIAL_WORKER, CanaryRoute::Lan),
            ],
            &estimate,
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::TimingIdentityMismatch));
    }

    #[test]
    fn pulp_slug_without_immutable_repository_identity_is_out_of_scope() {
        let mut fixture = fixture();
        fixture.manifest.source.repository_id = PULP_REPOSITORY_ID + 1;
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[
                host(INITIAL_BUILDER, CanaryRoute::SameHost),
                host(INITIAL_WORKER, CanaryRoute::Lan),
            ],
            &timing(&fixture, 120_000),
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::WrongScope));
    }

    #[test]
    fn target_and_persistent_staging_are_independently_required() {
        let fixture = fixture();
        let mut estimate = timing(&fixture, 120_000);
        estimate.target = "release-mac".to_owned();
        let mut worker = host(INITIAL_WORKER, CanaryRoute::Lan);
        worker.staging_root = "/private/tmp/shipyard".to_owned();
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[host(INITIAL_BUILDER, CanaryRoute::SameHost), worker],
            &estimate,
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::WrongScope));
        assert!(reasons.contains(&CanaryIneligibleReason::StagingRootInvalid));
    }

    #[test]
    fn macos_staging_paths_are_validated_lexically() {
        assert!(canonical_absolute_macos_path("/var/lib/shipyard/m1"));
        assert!(!canonical_absolute_macos_path("/var//lib/shipyard/m1"));
        assert!(!canonical_absolute_macos_path("/var/./lib/shipyard/m1"));
        assert!(!canonical_absolute_macos_path("/var/lib/../shipyard/m1"));
        assert!(!canonical_absolute_macos_path("/var/lib/shipyard/m1/"));
        assert!(macos_path_is_within(
            "/private/tmp/shipyard",
            "/private/tmp"
        ));
        assert!(!macos_path_is_within(
            "/private/tmp-safe/shipyard",
            "/private/tmp"
        ));
    }

    #[test]
    fn fractional_overhead_above_limit_fails_closed() {
        let fixture = fixture();
        let mut estimate = timing(&fixture, 159);
        estimate.single_host_ms = 1_500_000;
        estimate.distributed_shard_ms = 1_000;
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[
                host(INITIAL_BUILDER, CanaryRoute::SameHost),
                host(INITIAL_WORKER, CanaryRoute::Lan),
            ],
            &estimate,
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::TransferOverheadTooHigh));
    }

    #[test]
    fn savings_percentage_is_exact_and_capability_bound_matches_assignments() {
        let fixture = fixture();
        let mut estimate = timing(&fixture, 0);
        estimate.single_host_ms = 1_200_001;
        estimate.distributed_shard_ms = 1_080_001;
        let mut worker = host(INITIAL_WORKER, CanaryRoute::Lan);
        worker.capabilities = (0..MAX_CAPABILITIES)
            .map(|index| format!("capability-{index:03}"))
            .chain(std::iter::once("macos-arm64".to_owned()))
            .collect();
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[host(INITIAL_BUILDER, CanaryRoute::SameHost), worker],
            &estimate,
        )
        .expect("decision");
        let PulpMacCanaryDecision::Ineligible { reasons } = decision else {
            panic!("expected ineligible");
        };
        assert!(reasons.contains(&CanaryIneligibleReason::BenefitTooSmall));
        assert!(reasons.contains(&CanaryIneligibleReason::CapabilityMismatch));
    }

    #[test]
    fn large_eligible_estimate_reports_unsaturated_overhead() {
        let fixture = fixture();
        let mut estimate = timing(&fixture, 2_000_000_000_000_000_000);
        estimate.single_host_ms = 18_000_000_000_000_000_000;
        estimate.distributed_shard_ms = 13_500_000_000_000_000_000;
        let decision = assess_pulp_mac_canary(
            proof(&fixture),
            &policy(),
            &[
                host(INITIAL_BUILDER, CanaryRoute::SameHost),
                host(INITIAL_WORKER, CanaryRoute::Lan),
            ],
            &estimate,
        )
        .expect("decision");
        let PulpMacCanaryDecision::Eligible {
            predicted_overhead_percent,
            ..
        } = decision
        else {
            panic!("expected eligible");
        };
        assert_eq!(predicted_overhead_percent, 14);
    }
}
