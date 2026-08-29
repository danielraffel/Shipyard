use std::collections::VecDeque;

use tempfile::TempDir;

use super::*;
use crate::parallel_proof_canary::{CanaryRoute, CanaryStagingClass};
use crate::parallel_proof_canary_remote_cache::{
    RemoteM1CacheAuthority, synthetic_cache_generation_manifest, test_remote_authority_receipt,
};

fn persistent_temp() -> TempDir {
    let current = std::env::current_dir().unwrap().canonicalize().unwrap();
    tempfile::Builder::new()
        .prefix(".shipyard-cache-test-")
        .tempdir_in(current)
        .unwrap()
}

fn cache_tree() -> TempDir {
    let root = persistent_temp();
    fs::create_dir(root.path().join("nested")).unwrap();
    fs::write(root.path().join("index.bin"), b"cache-index").unwrap();
    fs::write(root.path().join("nested/object.bin"), b"cache-object").unwrap();
    root
}

fn policy(manifest: &CacheGenerationManifest) -> PulpMacCanaryPolicy {
    PulpMacCanaryPolicy {
        enabled: true,
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        assessed_at_ms: 1_000,
        maximum_observation_age_ms: 100,
        required_cache_generations: vec![manifest.generation.clone()],
        ..PulpMacCanaryPolicy::default()
    }
}

fn host_digest(host_id: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(format!("host:{host_id}").as_bytes())
}

fn remote_authority(observed_at_ms: u64) -> RemoteM1CacheAuthority {
    RemoteM1CacheAuthority {
        source_host_id: "m3".to_owned(),
        host_id: "m1".to_owned(),
        host_observation_sha256: host_digest("m1"),
        host_session_generation: 7,
        route: CanaryRoute::Lan,
        destination: "shipyard@m1.local".to_owned(),
        known_hosts_sha256: Sha256Digest::of_bytes(b"known-hosts"),
        capabilities: vec!["macos-arm64".to_owned()],
        staging_root: "/Users/test/shipyard-staging".to_owned(),
        staging_class: CanaryStagingClass::Persistent,
        free_bytes: 10,
        artifact_bytes_total: 1,
        minimum_reserve_bytes: 1,
        terminal_instance_sha256: Sha256Digest::of_bytes(b"terminal"),
        companion_executable_sha256: Sha256Digest::of_bytes(b"companion"),
        observed_at_ms,
        model_calls: 0,
    }
}

fn receipt(
    host_id: &str,
    root: &Path,
    manifest: CacheGenerationManifest,
    observed_at_ms: u64,
) -> CacheGenerationObservationReceipt {
    let cache_root = root.to_str().unwrap().to_owned();
    let remote_authority = (host_id == "m1").then(|| {
        test_remote_authority_receipt(
            remote_authority(observed_at_ms),
            &cache_root,
            &manifest,
            observed_at_ms,
            1,
        )
    });
    CacheGenerationObservationReceipt {
        schema_version: CACHE_GENERATION_OBSERVATION_SCHEMA,
        host_id: host_id.to_owned(),
        host_observation_sha256: host_digest(host_id),
        observed_at_ms,
        probe_elapsed_ms: 1,
        cache_root,
        manifest_sha256: manifest.digest().unwrap(),
        manifest,
        remote_authority,
        model_calls: 0,
    }
}

#[derive(Default)]
struct FakeObserver {
    outputs: VecDeque<Result<CacheGenerationObservationReceipt, CacheObserverError>>,
    calls: Vec<String>,
    now_ms: u64,
}

impl CacheGenerationObserver for FakeObserver {
    fn observe(
        &mut self,
        spec: &CacheGenerationProbeSpec,
    ) -> Result<CacheGenerationObservationReceipt, CacheObserverError> {
        self.calls.push(spec.host_id().to_owned());
        self.outputs.pop_front().expect("fake cache observation")
    }

    fn controller_now_ms(&mut self) -> Result<u64, CacheObserverError> {
        Ok(self.now_ms)
    }
}

#[cfg(unix)]
#[test]
fn manifest_is_deterministic_complete_and_read_only() {
    let root = cache_tree();
    let before = fs::read(root.path().join("index.bin")).unwrap();
    let first = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let second = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    assert_eq!(first, second);
    assert_eq!(first.entries.len(), 3);
    assert_eq!(first.total_bytes, 23);
    assert_eq!(first.model_calls, 0);
    assert_eq!(fs::read(root.path().join("index.bin")).unwrap(), before);
    first.validate().unwrap();

    fs::write(root.path().join("nested/object.bin"), b"different-object").unwrap();
    let changed = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    assert_ne!(first.generation.sha256, changed.generation.sha256);
}

#[cfg(not(unix))]
#[test]
fn manifest_production_refuses_without_no_follow_directory_handles() {
    let root = cache_tree();
    let error = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap_err();
    assert!(matches!(
        error,
        CacheObserverError::Artifact(message)
            if message.contains("requires no-follow directory handles")
    ));
}

#[cfg(unix)]
#[test]
fn manifest_identity_includes_the_cache_root_mode() {
    use std::os::unix::fs::PermissionsExt;

    let root = cache_tree();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let private = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o750)).unwrap();
    let shared = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    assert_eq!(private.root_mode, 0o700);
    assert_eq!(shared.root_mode, 0o750);
    assert_ne!(private.generation.sha256, shared.generation.sha256);
}

#[cfg(unix)]
#[test]
fn manifest_rejects_links_and_special_or_empty_trees() {
    use std::os::unix::fs::symlink;

    let empty = persistent_temp();
    assert!(produce_cache_generation_manifest(empty.path(), "skia", "m124").is_err());

    let linked = persistent_temp();
    fs::write(linked.path().join("target"), b"target").unwrap();
    symlink("target", linked.path().join("link")).unwrap();
    assert!(produce_cache_generation_manifest(linked.path(), "skia", "m124").is_err());
}

#[cfg(unix)]
#[test]
fn local_observer_requires_the_exact_immutable_manifest() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let spec =
        CacheGenerationProbeSpec::new("m3", host_digest("m3"), root.path(), manifest.clone())
            .unwrap();
    let mut observer = LocalCacheGenerationObserver::new("m3").unwrap();
    let receipt = observer.observe(&spec).unwrap();
    assert_eq!(receipt.manifest, manifest);
    assert_eq!(receipt.model_calls, 0);
    receipt.validate().unwrap();

    fs::write(root.path().join("index.bin"), b"drifted").unwrap();
    assert!(matches!(
        observer.observe(&spec),
        Err(CacheObserverError::GenerationMismatch { .. })
    ));
}

#[test]
fn local_observer_refuses_a_different_host_before_platform_packing() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let spec =
        CacheGenerationProbeSpec::new("m1", host_digest("m1"), root.path(), manifest).unwrap();
    let mut observer = LocalCacheGenerationObserver::new("m3").unwrap();

    assert!(matches!(
        observer.observe(&spec),
        Err(CacheObserverError::Invalid(message))
            if message == "local cache observer host binding"
    ));
}

#[cfg(not(unix))]
#[test]
fn local_observer_refuses_matching_host_without_no_follow_directory_handles() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let spec =
        CacheGenerationProbeSpec::new("m3", host_digest("m3"), root.path(), manifest).unwrap();
    let mut observer = LocalCacheGenerationObserver::new("m3").unwrap();

    assert!(matches!(
        observer.observe(&spec),
        Err(CacheObserverError::Artifact(message))
            if message.contains("requires no-follow directory handles")
    ));
}

#[test]
fn disabled_probe_calls_neither_observer_nor_cache() {
    let parent = persistent_temp();
    let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
    let request = PulpMacCacheProbeRequest {
        enabled: false,
        correlation_id: "disabled".to_owned(),
        builder: Vec::new(),
        worker: Vec::new(),
    };
    let mut observer = FakeObserver::default();
    let outcome = drive_pulp_mac_cache_probe(
        &request,
        &PulpMacCanaryPolicy::default(),
        &mut observer,
        &store,
    )
    .unwrap();
    assert_eq!(outcome, PulpMacCacheProbeOutcome::Disabled);
    assert!(observer.calls.is_empty());
}

#[test]
fn probe_is_m3_first_crash_durable_and_exactly_replayable() {
    let builder_root = cache_tree();
    let worker_root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let policy = policy(&manifest);
    let request = PulpMacCacheProbeRequest {
        enabled: true,
        correlation_id: "cache-proof-1".to_owned(),
        builder: vec![
            CacheGenerationProbeSpec::new(
                "m3",
                host_digest("m3"),
                builder_root.path(),
                manifest.clone(),
            )
            .unwrap(),
        ],
        worker: vec![
            CacheGenerationProbeSpec::new(
                "m1",
                host_digest("m1"),
                worker_root.path(),
                manifest.clone(),
            )
            .unwrap(),
        ],
    };
    let mut observer = FakeObserver {
        outputs: VecDeque::from([
            Ok(receipt("m3", builder_root.path(), manifest.clone(), 990)),
            Ok(receipt("m1", worker_root.path(), manifest, 995)),
        ]),
        now_ms: 1_000,
        ..FakeObserver::default()
    };
    let parent = persistent_temp();
    let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
    let PulpMacCacheProbeOutcome::Recorded {
        evidence,
        write_outcome,
    } = drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).unwrap()
    else {
        panic!("expected recorded cache proof");
    };
    assert_eq!(write_outcome, StoreWriteOutcome::Created);
    assert_eq!(observer.calls, ["m3", "m1"]);
    assert!(evidence.proves_policy(&policy));
    assert!(evidence.proves_policy_and_hosts(&policy, &host_digest("m3"), &host_digest("m1")));
    assert_eq!(evidence.model_calls, 0);
    evidence.digest(&policy).unwrap();

    let replay = drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).unwrap();
    assert!(matches!(
        replay,
        PulpMacCacheProbeOutcome::Recorded {
            write_outcome: StoreWriteOutcome::AlreadyPresent,
            ..
        }
    ));
    assert_eq!(observer.calls, ["m3", "m1"]);
    assert_eq!(store.load("cache-proof-1", &policy).unwrap(), *evidence);

    let mut rebound_request = request;
    rebound_request.builder[0].host_observation_sha256 =
        Sha256Digest::of_bytes(b"new-m3-host-observation");
    assert!(matches!(
        drive_pulp_mac_cache_probe(&rebound_request, &policy, &mut observer, &store),
        Err(CacheObserverError::ImmutableConflict(key)) if key == "cache-proof-1"
    ));
    assert_eq!(observer.calls, ["m3", "m1"]);
}

#[test]
fn remote_authority_accepts_manifest_specific_transport_receipts() {
    let root = cache_tree();
    let first = synthetic_cache_generation_manifest("first", "v1");
    let second = synthetic_cache_generation_manifest("second", "v1");
    let policy = PulpMacCanaryPolicy {
        enabled: true,
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        assessed_at_ms: 1_000,
        maximum_observation_age_ms: 100,
        required_cache_generations: vec![first.generation.clone(), second.generation.clone()],
        ..PulpMacCanaryPolicy::default()
    };
    let evidence = PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: "multi-generation-authority".to_owned(),
        repository_id: policy.repository_id,
        repository: policy.repository.clone(),
        target: policy.target.clone(),
        target_triple: policy.target_triple.clone(),
        builder_host_id: policy.builder_host_id.clone(),
        worker_host_id: policy.worker_host_id.clone(),
        assessed_at_ms: 1_000,
        builder: vec![
            receipt("m3", root.path(), first.clone(), 990),
            receipt("m3", root.path(), second.clone(), 991),
        ],
        worker: vec![
            receipt("m1", root.path(), first, 995),
            receipt("m1", root.path(), second, 996),
        ],
        model_calls: 0,
    };

    let authority = evidence
        .remote_worker_authority(&policy, &host_digest("m1"), 1)
        .expect("each manifest-bound transport may differ under one controller fence");
    assert_eq!(authority.authority.host_session_generation, 7);
}

#[test]
fn persisted_remote_authority_refuses_a_different_builder_source() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let policy = policy(&manifest);
    let mut worker = receipt("m1", root.path(), manifest.clone(), 995);
    let mut authority = remote_authority(995);
    authority.source_host_id = "other-builder".to_owned();
    worker.remote_authority = Some(test_remote_authority_receipt(
        authority,
        root.path().to_str().unwrap(),
        &manifest,
        995,
        1,
    ));
    let evidence = PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: "wrong-remote-source".to_owned(),
        repository_id: policy.repository_id,
        repository: policy.repository.clone(),
        target: policy.target.clone(),
        target_triple: policy.target_triple.clone(),
        builder_host_id: policy.builder_host_id.clone(),
        worker_host_id: policy.worker_host_id.clone(),
        assessed_at_ms: 1_000,
        builder: vec![receipt("m3", root.path(), manifest, 990)],
        worker: vec![worker],
        model_calls: 0,
    };

    assert!(matches!(
        evidence.validate(&policy),
        Err(CacheObserverError::Invalid(field))
            if field == "remote cache authority source host"
    ));
    assert!(
        evidence
            .remote_worker_authority(&policy, &host_digest("m1"), 1)
            .is_none()
    );
}

#[test]
fn tailnet_cache_measurement_does_not_mint_lan_worker_authority() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let policy = PulpMacCanaryPolicy {
        enabled: true,
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        assessed_at_ms: 1_000,
        maximum_observation_age_ms: 100,
        required_cache_generations: vec![manifest.generation.clone()],
        ..PulpMacCanaryPolicy::default()
    };
    let mut authority = remote_authority(995);
    authority.route = CanaryRoute::Tailnet;
    authority.destination = "shipyard@m1.tailnet".to_owned();
    let mut worker = receipt("m1", root.path(), manifest.clone(), 995);
    worker.remote_authority = Some(test_remote_authority_receipt(
        authority,
        root.path().to_str().unwrap(),
        &manifest,
        995,
        1,
    ));
    let evidence = PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: "tailnet-measurement-only".to_owned(),
        repository_id: policy.repository_id,
        repository: policy.repository.clone(),
        target: policy.target.clone(),
        target_triple: policy.target_triple.clone(),
        builder_host_id: policy.builder_host_id.clone(),
        worker_host_id: policy.worker_host_id.clone(),
        assessed_at_ms: 1_000,
        builder: vec![receipt("m3", root.path(), manifest, 990)],
        worker: vec![worker],
        model_calls: 0,
    };

    assert!(evidence.validate(&policy).is_ok());
    assert!(
        evidence
            .remote_worker_authority(&policy, &host_digest("m1"), 1)
            .is_none()
    );
}

#[test]
fn failed_builder_proof_never_observes_worker() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let request = PulpMacCacheProbeRequest {
        enabled: true,
        correlation_id: "builder-failed".to_owned(),
        builder: vec![
            CacheGenerationProbeSpec::new("m3", host_digest("m3"), root.path(), manifest.clone())
                .unwrap(),
        ],
        worker: vec![
            CacheGenerationProbeSpec::new("m1", host_digest("m1"), root.path(), manifest.clone())
                .unwrap(),
        ],
    };
    let mut observer = FakeObserver {
        outputs: VecDeque::from([Err(CacheObserverError::GenerationMismatch {
            host_id: "m3".to_owned(),
            cache_name: "skia".to_owned(),
        })]),
        now_ms: 1_000,
        ..FakeObserver::default()
    };
    let parent = persistent_temp();
    let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
    assert!(
        drive_pulp_mac_cache_probe(&request, &policy(&manifest), &mut observer, &store).is_err()
    );
    assert_eq!(observer.calls, ["m3"]);
}

#[test]
fn stale_builder_proof_never_observes_worker() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let request = PulpMacCacheProbeRequest {
        enabled: true,
        correlation_id: "builder-stale".to_owned(),
        builder: vec![
            CacheGenerationProbeSpec::new("m3", host_digest("m3"), root.path(), manifest.clone())
                .unwrap(),
        ],
        worker: vec![
            CacheGenerationProbeSpec::new("m1", host_digest("m1"), root.path(), manifest.clone())
                .unwrap(),
        ],
    };
    let mut observer = FakeObserver {
        outputs: VecDeque::from([
            Ok(receipt("m3", root.path(), manifest.clone(), 899)),
            Ok(receipt("m1", root.path(), manifest.clone(), 995)),
        ]),
        now_ms: 1_000,
        ..FakeObserver::default()
    };
    let parent = persistent_temp();
    let store = PulpMacCacheEvidenceStore::open(parent.path().join("evidence")).unwrap();
    assert!(
        drive_pulp_mac_cache_probe(&request, &policy(&manifest), &mut observer, &store).is_err()
    );
    assert_eq!(observer.calls, ["m3"]);
}

#[test]
fn stale_or_wrong_inventory_never_proves_policy() {
    let root = cache_tree();
    let manifest = synthetic_cache_generation_manifest("skia", "m124");
    let policy = policy(&manifest);
    let mut evidence = PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: "stale".to_owned(),
        repository_id: policy.repository_id,
        repository: policy.repository.clone(),
        target: policy.target.clone(),
        target_triple: policy.target_triple.clone(),
        builder_host_id: policy.builder_host_id.clone(),
        worker_host_id: policy.worker_host_id.clone(),
        assessed_at_ms: 1_000,
        builder: vec![receipt("m3", root.path(), manifest.clone(), 899)],
        worker: vec![receipt("m1", root.path(), manifest, 995)],
        model_calls: 0,
    };
    assert!(!evidence.proves_policy(&policy));
    evidence.builder[0].observed_at_ms = 990;
    evidence.worker[0].manifest.generation.generation = "other".to_owned();
    assert!(!evidence.proves_policy(&policy));

    evidence.worker[0].manifest.generation.generation = "m124".to_owned();
    evidence.worker[0].manifest_sha256 = evidence.worker[0].manifest.digest().unwrap();
    let later_policy = PulpMacCanaryPolicy {
        assessed_at_ms: 1_100,
        ..policy
    };
    assert!(!evidence.proves_policy(&later_policy));
}
