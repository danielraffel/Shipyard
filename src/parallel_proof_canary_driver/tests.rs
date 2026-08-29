use super::*;
use crate::parallel_proof::{
    ArtifactIdentity, ArtifactTrustClass, BuildIdentity, ExecutionBoundary, ParallelProofManifest,
    ProofSubject, ShardPlan, SourceIdentity, TestCase, TestInventory, TrustIdentity,
};
use crate::parallel_proof_canary::{CanaryRoute, CanaryStagingClass};

struct Fixture {
    inventory: TestInventory,
    plan: ShardPlan,
    manifest: ParallelProofManifest,
}

impl Fixture {
    fn proof(&self) -> ParallelProofContext<'_> {
        ParallelProofContext::new(&self.manifest, &self.inventory, &self.plan).unwrap()
    }
}

fn digest(value: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(value.as_bytes())
}

fn fixture() -> Fixture {
    let inventory = TestInventory::new(vec![
        TestCase {
            id: "audio".to_owned(),
            dependencies: vec![],
            fixture_setup: vec![],
            fixture_required: vec![],
            fixture_cleanup: vec![],
            run_serial: false,
            resource_locks: vec![],
            required_capabilities: vec!["macos-arm64".to_owned()],
        },
        TestCase {
            id: "dsp".to_owned(),
            dependencies: vec![],
            fixture_setup: vec![],
            fixture_required: vec![],
            fixture_cleanup: vec![],
            run_serial: false,
            resource_locks: vec![],
            required_capabilities: vec!["macos-arm64".to_owned()],
        },
    ])
    .unwrap();
    let plan = ShardPlan::deterministic_balanced(&inventory, 2).unwrap();
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
            subject: ProofSubject::PullRequest { number: 88 },
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
    .unwrap();
    Fixture {
        inventory,
        plan,
        manifest,
    }
}

fn different_fixture() -> Fixture {
    let mut different = fixture();
    let mut source = different.manifest.source.clone();
    source.head_sha = "c".repeat(64);
    different.manifest = ParallelProofManifest::new(
        source,
        different.manifest.build.clone(),
        different.manifest.artifact.clone(),
        different.manifest.trust.clone(),
        &different.inventory,
        &different.plan,
    )
    .unwrap();
    different
}

fn cache() -> CanaryCacheGeneration {
    CanaryCacheGeneration {
        name: "skia".to_owned(),
        generation: "m124-arm64".to_owned(),
        sha256: digest("skia"),
    }
}

fn hosts(free_bytes: u64) -> Vec<CanaryHostObservation> {
    vec![
        CanaryHostObservation {
            host_id: "m3".to_owned(),
            online: true,
            observed_at_ms: 9_500,
            session_generation: 11,
            route: CanaryRoute::SameHost,
            staging_root: "/var/lib/shipyard/m3".to_owned(),
            staging_class: CanaryStagingClass::Persistent,
            free_bytes,
            capabilities: vec!["macos-arm64".to_owned()],
            cache_generations: vec![cache()],
        },
        CanaryHostObservation {
            host_id: "m1".to_owned(),
            online: true,
            observed_at_ms: 9_500,
            session_generation: 22,
            route: CanaryRoute::Lan,
            staging_root: "/var/lib/shipyard/m1".to_owned(),
            staging_class: CanaryStagingClass::Persistent,
            free_bytes,
            capabilities: vec!["macos-arm64".to_owned()],
            cache_generations: vec![cache()],
        },
    ]
}

fn policy(enabled: bool) -> PulpMacCanaryPolicy {
    PulpMacCanaryPolicy {
        enabled,
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        assessed_at_ms: 10_000,
        maximum_observation_age_ms: 1_000,
        minimum_free_bytes: 500,
        required_cache_generations: vec![cache()],
    }
}

fn timing(fixture: &Fixture) -> CanaryTimingEstimate {
    CanaryTimingEstimate {
        manifest_digest: fixture
            .manifest
            .digest(&fixture.inventory, &fixture.plan)
            .unwrap(),
        target: "mac".to_owned(),
        single_host_ms: 1_000_000,
        distributed_shard_ms: 600_000,
        transfer_and_dispatch_ms: 30_000,
    }
}

fn distributed() -> DistributedExecutionObservation {
    DistributedExecutionObservation {
        delivery: ArtifactDeliveryObservation {
            mode: ArtifactDeliveryMode::VerifiedPrefixResume,
            artifact_bytes_total: 1_000,
            artifact_bytes_reused: 400,
            artifact_bytes_transferred: 600,
            interruption: Some(InterruptedTransferEvidence {
                interrupted_partial_sha256: digest("partial"),
                verified_prefix_sha256: digest("prefix"),
                bytes_before_interruption: 500,
                verified_resume_offset_bytes: 400,
                bytes_after_resume: 600,
            }),
        },
        setup_ms: 1_000,
        transfer_ms: 2_000,
        verification_ms: 1_000,
        dispatch_ms: 1_000,
        shard_execution_ms: 500_000,
        worker_active_ms: 800_000,
        submit_to_receipt_ms: 505_000,
        caches: vec![ObservedCacheUse {
            generation: cache(),
            usage: CacheUse::Hit,
        }],
    }
}

struct FakeExecutor {
    observations: Vec<Vec<CanaryHostObservation>>,
    calls: Vec<&'static str>,
    distributed: DistributedExecutionObservation,
    corrupt_control: bool,
}

impl FakeExecutor {
    fn normal() -> Self {
        Self {
            observations: vec![hosts(10_000), hosts(10_000), hosts(10_000)],
            calls: vec![],
            distributed: distributed(),
            corrupt_control: false,
        }
    }
}

impl PulpMacCanaryExecutor for FakeExecutor {
    fn controller_now_ms(&mut self) -> Result<u64, ParallelProofError> {
        self.calls.push("now");
        Ok(10_000)
    }

    fn authenticated_host_observations(
        &mut self,
    ) -> Result<Vec<CanaryHostObservation>, ParallelProofError> {
        self.calls.push("observe");
        if self.observations.is_empty() {
            return Err(ParallelProofError::InvalidField("test observations"));
        }
        Ok(self.observations.remove(0))
    }

    fn run_single_host_control(
        &mut self,
        proof: ParallelProofContext<'_>,
        host: &CanaryHostObservation,
    ) -> Result<SingleHostControlReceipt, ParallelProofError> {
        self.calls.push("control");
        let mut receipt =
            SingleHostControlReceipt::capture(proof, &policy(true), host, 800_000, 790_000, 0)?;
        if self.corrupt_control {
            receipt.artifact_sha256 = digest("wrong-artifact");
        }
        Ok(receipt)
    }

    fn run_distributed_shadow(
        &mut self,
        _manifest_digest: &Sha256Digest,
    ) -> Result<DistributedExecutionObservation, ParallelProofError> {
        self.calls.push("distributed");
        Ok(self.distributed.clone())
    }
}

fn store(directory: &tempfile::TempDir) -> PulpMacCanaryEvidenceStore {
    PulpMacCanaryEvidenceStore::open(directory.path().join("canary")).unwrap()
}

#[test]
fn disabled_policy_never_calls_executor() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    let outcome = drive_pulp_mac_canary(
        fixture.proof(),
        &policy(false),
        &timing(&fixture),
        "disabled",
        &mut executor,
        &store(&temporary),
    )
    .unwrap();
    assert_eq!(outcome, PulpMacCanaryDriverOutcome::Disabled);
    assert!(executor.calls.is_empty());
}

#[test]
fn ineligible_observation_never_executes_work() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor.observations[0][1].online = false;
    let outcome = drive_pulp_mac_canary(
        fixture.proof(),
        &policy(true),
        &timing(&fixture),
        "offline",
        &mut executor,
        &store(&temporary),
    )
    .unwrap();
    assert!(matches!(outcome, PulpMacCanaryDriverOutcome::Ineligible(_)));
    assert_eq!(executor.calls, vec!["observe", "now"]);
}

#[test]
fn executes_control_before_transfer_and_records_actual_resume_counters() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let evidence_store = store(&temporary);
    let mut executor = FakeExecutor::normal();
    let outcome = drive_pulp_mac_canary(
        fixture.proof(),
        &policy(true),
        &timing(&fixture),
        "resume-1",
        &mut executor,
        &evidence_store,
    )
    .unwrap();
    assert_eq!(
        executor.calls,
        vec![
            "observe",
            "now",
            "control",
            "observe",
            "now",
            "distributed",
            "observe",
            "now"
        ]
    );
    let PulpMacCanaryDriverOutcome::Recorded {
        evidence,
        write_outcome,
    } = outcome
    else {
        panic!("expected evidence");
    };
    assert_eq!(write_outcome, StoreWriteOutcome::Created);
    assert_eq!(evidence.receipt.artifact_bytes_reused, 400);
    assert_eq!(evidence.receipt.artifact_bytes_transferred, 600);
    assert_eq!(evidence.receipt.model_calls, 0);
    assert_eq!(evidence.receipt.caches[0].claimed_bytes_avoided, 0);
    let mut tampered_fence = (*evidence).clone();
    tampered_fence.final_host_observations[1].free_bytes -= 1;
    assert!(matches!(
        tampered_fence.validate(),
        Err(ParallelProofError::CorruptRecord(_))
    ));
    assert_eq!(
        evidence
            .interrupted_transfer
            .unwrap()
            .bytes_before_interruption,
        500
    );
    assert_eq!(
        evidence_store.load("resume-1").unwrap(),
        evidence_store.load("resume-1").unwrap()
    );
}

#[test]
fn immutable_replay_is_idempotent_without_reexecution_and_conflicts_fail_closed() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let evidence_store = store(&temporary);
    let mut first = FakeExecutor::normal();
    let first_outcome = drive_pulp_mac_canary(
        fixture.proof(),
        &policy(true),
        &timing(&fixture),
        "same-key",
        &mut first,
        &evidence_store,
    )
    .unwrap();
    assert!(matches!(
        first_outcome,
        PulpMacCanaryDriverOutcome::Recorded {
            write_outcome: StoreWriteOutcome::Created,
            ..
        }
    ));
    let mut replay = FakeExecutor::normal();
    let replay_outcome = drive_pulp_mac_canary(
        fixture.proof(),
        &policy(true),
        &timing(&fixture),
        "same-key",
        &mut replay,
        &evidence_store,
    )
    .unwrap();
    assert!(matches!(
        replay_outcome,
        PulpMacCanaryDriverOutcome::Recorded {
            write_outcome: StoreWriteOutcome::AlreadyPresent,
            ..
        }
    ));
    assert!(replay.calls.is_empty());
    let other = different_fixture();
    let mut wrong_proof_replay = FakeExecutor::normal();
    assert!(matches!(
        drive_pulp_mac_canary(
            other.proof(),
            &policy(true),
            &timing(&other),
            "same-key",
            &mut wrong_proof_replay,
            &evidence_store,
        ),
        Err(ParallelProofError::ImmutableConflict(_))
    ));
    assert!(wrong_proof_replay.calls.is_empty());

    let mut wrong_scope = policy(true);
    wrong_scope.target = "release-mac".to_owned();
    let mut wrong_scope_replay = FakeExecutor::normal();
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &wrong_scope,
            &timing(&fixture),
            "same-key",
            &mut wrong_scope_replay,
            &evidence_store,
        ),
        Err(ParallelProofError::ImmutableConflict(_))
    ));
    assert!(wrong_scope_replay.calls.is_empty());

    let conflict_directory = tempfile::tempdir().unwrap();
    let conflict_store = store(&conflict_directory);
    let mut conflict = FakeExecutor::normal();
    conflict.distributed.worker_active_ms += 1;
    let PulpMacCanaryDriverOutcome::Recorded {
        evidence: conflicting_evidence,
        ..
    } = drive_pulp_mac_canary(
        fixture.proof(),
        &policy(true),
        &timing(&fixture),
        "same-key",
        &mut conflict,
        &conflict_store,
    )
    .unwrap()
    else {
        panic!("expected conflicting evidence");
    };
    assert!(matches!(
        evidence_store.record(&conflicting_evidence),
        Err(ParallelProofError::ImmutableConflict(_))
    ));
}

#[test]
fn session_change_before_transfer_fails_closed() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor.observations[1][1].session_generation += 1;
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "session-change",
            &mut executor,
            &store(&temporary),
        ),
        Err(ParallelProofError::BindingMismatch("canary host fence"))
    ));
    assert_eq!(
        executor.calls,
        vec!["observe", "now", "control", "observe", "now"]
    );
}

#[test]
fn control_receipt_must_bind_exact_proof_and_session() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor.corrupt_control = true;
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "wrong-control",
            &mut executor,
            &store(&temporary),
        ),
        Err(ParallelProofError::BindingMismatch(
            "single-host control receipt"
        ))
    ));
    assert_eq!(executor.calls, vec!["observe", "now", "control"]);
}

#[test]
fn reserve_loss_after_execution_blocks_publication() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor.observations[2] = hosts(499);
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "reserve-loss",
            &mut executor,
            &store(&temporary),
        ),
        Err(ParallelProofError::BindingMismatch(
            "canary storage reserve"
        ))
    ));
}

#[test]
fn stale_execution_fence_is_rejected_after_control() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor.observations[1][0].observed_at_ms = 8_999;
    executor.observations[1][1].observed_at_ms = 8_999;
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "stale-fence",
            &mut executor,
            &store(&temporary),
        ),
        Err(ParallelProofError::BindingMismatch("canary host fence"))
    ));
    assert_eq!(
        executor.calls,
        vec!["observe", "now", "control", "observe", "now"]
    );
}

#[test]
fn forged_resume_counters_are_rejected() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor
        .distributed
        .delivery
        .interruption
        .as_mut()
        .unwrap()
        .bytes_after_resume = 599;
    let evidence_store = store(&temporary);
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "bad-resume",
            &mut executor,
            &evidence_store,
        ),
        Err(ParallelProofError::InvalidField("canary delivery evidence"))
    ));
    let mut retry = FakeExecutor::normal();
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "bad-resume",
            &mut retry,
            &evidence_store,
        ),
        Err(ParallelProofError::InvalidAttemptSequence(_))
    ));
    assert!(retry.calls.is_empty());
}

#[test]
fn full_transfer_and_immutable_reuse_derive_exact_offsets() {
    let fixture = fixture();
    for (correlation, mode, reused, transferred, expected_offset) in [
        ("full", ArtifactDeliveryMode::FullTransfer, 0, 1_000, 0),
        (
            "reuse",
            ArtifactDeliveryMode::ImmutableObjectReuse,
            1_000,
            0,
            1_000,
        ),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let mut executor = FakeExecutor::normal();
        executor.distributed.delivery.mode = mode;
        executor.distributed.delivery.artifact_bytes_reused = reused;
        executor.distributed.delivery.artifact_bytes_transferred = transferred;
        executor.distributed.delivery.interruption = None;
        let outcome = drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            correlation,
            &mut executor,
            &store(&temporary),
        )
        .unwrap();
        let PulpMacCanaryDriverOutcome::Recorded { evidence, .. } = outcome else {
            panic!("expected recorded evidence");
        };
        assert_eq!(
            evidence.receipt.verified_resume_offset_bytes,
            expected_offset
        );
    }
}

#[test]
fn cache_generation_must_match_authenticated_observation() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    executor.distributed.caches[0].generation.generation = "forged".to_owned();
    assert!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "bad-cache",
            &mut executor,
            &store(&temporary),
        )
        .is_err()
    );
}

#[test]
fn public_measurement_types_do_not_expose_claimed_avoided_bytes() {
    let observed = ObservedCacheUse {
        generation: cache(),
        usage: CacheUse::Hit,
    };
    assert_eq!(observed.usage, CacheUse::Hit);
}

#[test]
fn malformed_correlation_id_is_rejected_before_executor_calls() {
    let fixture = fixture();
    let temporary = tempfile::tempdir().unwrap();
    let mut executor = FakeExecutor::normal();
    assert!(matches!(
        drive_pulp_mac_canary(
            fixture.proof(),
            &policy(true),
            &timing(&fixture),
            "bad correlation",
            &mut executor,
            &store(&temporary),
        ),
        Err(ParallelProofError::InvalidField(
            "canary measurement correlation id"
        ))
    ));
    assert!(executor.calls.is_empty());
}
