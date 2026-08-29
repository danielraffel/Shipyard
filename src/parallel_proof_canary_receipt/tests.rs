use super::*;
use crate::parallel_proof::{
    ArtifactIdentity, ArtifactTrustClass, BuildIdentity, ExecutionBoundary, ParallelProofManifest,
    ProofSubject, ShardPlan, SourceIdentity, TestCase, TestInventory, TrustIdentity,
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
    ParallelProofContext::new(&fixture.manifest, &fixture.inventory, &fixture.plan).expect("proof")
}

fn policy() -> PulpMacCanaryPolicy {
    PulpMacCanaryPolicy {
        enabled: true,
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        ..PulpMacCanaryPolicy::default()
    }
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

fn input_for(fixture: &Fixture, policy: &PulpMacCanaryPolicy) -> CanaryMeasurementInput {
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
            policy,
            &host(&policy.builder_host_id, CanaryRoute::SameHost, 3),
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

fn input(fixture: &Fixture) -> CanaryMeasurementInput {
    input_for(fixture, &policy())
}

fn receipt(
    fixture: &Fixture,
    input: CanaryMeasurementInput,
) -> Result<PulpMacCanaryMeasurementReceipt, ParallelProofError> {
    receipt_for(fixture, &policy(), input)
}

fn receipt_for(
    fixture: &Fixture,
    policy: &PulpMacCanaryPolicy,
    input: CanaryMeasurementInput,
) -> Result<PulpMacCanaryMeasurementReceipt, ParallelProofError> {
    PulpMacCanaryMeasurementReceipt::capture(
        proof(fixture),
        policy,
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
            &policy(),
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
            &policy(),
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
            &policy(),
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
fn receipt_refuses_a_manifest_outside_the_configured_scope() {
    let mut fixture = fixture();
    let measurement = input(&fixture);
    fixture.manifest.source.repository_id = 99;
    fixture.manifest.source.repository = "example/project".to_owned();
    assert!(matches!(
        receipt(&fixture, measurement),
        Err(ParallelProofError::BindingMismatch(
            "parallel-proof canary scope"
        ))
    ));
}

#[test]
fn matching_non_pulp_scope_is_receipt_bound() {
    let mut fixture = fixture();
    fixture.manifest.source.repository_id = 42;
    fixture.manifest.source.repository = "example/project".to_owned();
    let configured = PulpMacCanaryPolicy {
        repository_id: 42,
        repository: "example/project".to_owned(),
        target: "release-mac".to_owned(),
        builder_host_id: "builder-a".to_owned(),
        worker_host_id: "worker-b".to_owned(),
        ..policy()
    };
    let builder = host("builder-a", CanaryRoute::SameHost, 4);
    let worker = host("worker-b", CanaryRoute::Lan, 7);
    let control = host("builder-a", CanaryRoute::SameHost, 3);
    let mut admitted = decision(&fixture);
    if let PulpMacCanaryDecision::Eligible {
        builder_host_id,
        worker_host_id,
        host_observations_digest,
        ..
    } = &mut admitted
    {
        *builder_host_id = builder.host_id.clone();
        *worker_host_id = worker.host_id.clone();
        *host_observations_digest =
            canary_host_observations_digest(&builder, &worker).expect("host digest");
    }
    let mut measurement = input_for(&fixture, &configured);
    measurement.single_host_control = SingleHostControlReceipt::capture(
        proof(&fixture),
        &configured,
        &control,
        1_000_000,
        900_000,
        0,
    )
    .expect("control");
    let receipt = PulpMacCanaryMeasurementReceipt::capture(
        proof(&fixture),
        &configured,
        &admitted,
        &builder,
        &worker,
        &control,
        measurement,
    )
    .expect("generic receipt");
    assert_eq!(receipt.repository_id, 42);
    assert_eq!(receipt.repository, "example/project");
    assert_eq!(receipt.target, "release-mac");
    assert_eq!(receipt.builder_host_id, "builder-a");
    assert_eq!(receipt.worker_host_id, "worker-b");
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
