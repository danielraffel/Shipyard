use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs;

use tempfile::TempDir;

use super::*;
use crate::parallel_proof_canary::PulpMacCanaryPolicy;
use crate::parallel_proof_canary::{CanaryRoute, CanaryStagingClass};
use crate::parallel_proof_canary_cache::{
    CACHE_GENERATION_OBSERVATION_SCHEMA, CacheGenerationObservationReceipt,
    PULP_MAC_CACHE_EVIDENCE_SCHEMA, PulpMacCacheProbeEvidence, produce_cache_generation_manifest,
};
use crate::parallel_proof_canary_remote_cache::{
    RemoteM1CacheAuthority, test_remote_authority_receipt,
};

fn pulp_policy() -> PulpMacCanaryPolicy {
    PulpMacCanaryPolicy {
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        ..PulpMacCanaryPolicy::default()
    }
}

fn cache_evidence(
    assessed_at_ms: u64,
    builder: Vec<CacheGenerationObservationReceipt>,
    worker: Vec<CacheGenerationObservationReceipt>,
) -> PulpMacCacheProbeEvidence {
    PulpMacCacheProbeEvidence {
        schema_version: PULP_MAC_CACHE_EVIDENCE_SCHEMA,
        correlation_id: "controller-cache-proof".to_owned(),
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        assessed_at_ms,
        builder,
        worker,
        model_calls: 0,
    }
}

#[derive(Default)]
struct FakeRunner {
    outputs: VecDeque<Result<ReadOnlyProbeOutput, CanaryObserverError>>,
    argv: Vec<Vec<OsString>>,
    known_host_authorities: Vec<String>,
}

impl ReadOnlyCanaryProbeRunner for FakeRunner {
    fn run(
        &mut self,
        command: &mut Command,
        _deadline: Instant,
        _label: &str,
    ) -> Result<ReadOnlyProbeOutput, CanaryObserverError> {
        self.argv.push(
            std::iter::once(command.get_program().to_os_string())
                .chain(command.get_args().map(OsString::from))
                .collect(),
        );
        if let Some(value) = command
            .get_envs()
            .find(|(key, _)| *key == KNOWN_HOSTS_ENV)
            .and_then(|(_, value)| value)
        {
            self.known_host_authorities
                .push(value.to_string_lossy().into_owned());
        }
        self.outputs.pop_front().expect("fake output")
    }
}

fn uuid() -> &'static str {
    "12345678-1234-1234-1234-123456789abc"
}

fn output(staging: &str) -> ReadOnlyProbeOutput {
    ReadOnlyProbeOutput::new(
        true,
        format!(
            "schema\t1\nplatform_uuid\t{}\nboot_seconds\t1787941145\nstaging\tpresent\ncanonical_root\t{}\nfree_kib\t2000000\n",
            uuid(), staging
        )
        .into_bytes(),
        Vec::new(),
    )
}

fn local_spec(staging: &str) -> ReadOnlyCanaryHostSpec {
    ReadOnlyCanaryHostSpec::new(
        "m3",
        ReadOnlyCanaryTarget::Local,
        Sha256Digest::of_bytes(uuid().as_bytes()),
        staging,
    )
    .unwrap()
}

fn observed_host(host_id: &str, staging: &str) -> ReadOnlyCanaryHostReceipt {
    let spec = ReadOnlyCanaryHostSpec::new(
        host_id,
        ReadOnlyCanaryTarget::Local,
        Sha256Digest::of_bytes(uuid().as_bytes()),
        staging,
    )
    .unwrap();
    StrictKnownHostCanaryObserver::with_runner(FakeRunner {
        outputs: VecDeque::from([Ok(output(staging))]),
        ..FakeRunner::default()
    })
    .observe(&spec, Duration::from_secs(1))
    .unwrap()
}

fn remote_cache_authority(
    host_observation_sha256: Sha256Digest,
    cache_root: &Path,
    manifest: &crate::parallel_proof_canary_cache::CacheGenerationManifest,
    staging_root: &str,
    assessed_at_ms: u64,
) -> crate::parallel_proof_canary_remote_cache::RemoteM1CacheAuthorityReceipt {
    test_remote_authority_receipt(
        RemoteM1CacheAuthority {
            source_host_id: "m3".to_owned(),
            host_id: "m1".to_owned(),
            host_observation_sha256,
            host_session_generation: 7,
            route: CanaryRoute::Lan,
            destination: "shipyard@m1.local".to_owned(),
            known_hosts_sha256: Sha256Digest::of_bytes(b"known-hosts"),
            capabilities: vec!["macos-arm64".to_owned()],
            staging_root: staging_root.to_owned(),
            staging_class: CanaryStagingClass::Persistent,
            free_bytes: 2_048_000_000,
            artifact_bytes_total: 4096,
            minimum_reserve_bytes: 1024,
            terminal_instance_sha256: Sha256Digest::of_bytes(b"terminal"),
            companion_executable_sha256: Sha256Digest::of_bytes(b"companion"),
            observed_at_ms: assessed_at_ms,
            model_calls: 0,
        },
        cache_root.to_str().unwrap(),
        manifest,
        assessed_at_ms,
        1,
    )
}

#[cfg(unix)]
#[test]
fn strict_ssh_observer_uses_only_explicit_read_only_authority() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let identity = temp.path().join("identity");
    fs::write(&known_hosts, "m1-lan ssh-ed25519 AAAATEST\n").unwrap();
    fs::write(&identity, "private-test-key").unwrap();
    let staging = "/Users/test/Library/Application Support/shipyard/canary-staging";
    let target =
        StrictSshCanaryTarget::new("/usr/bin/ssh", "m1-lan", &known_hosts, &identity, 22).unwrap();
    let spec = ReadOnlyCanaryHostSpec::new(
        "m1",
        ReadOnlyCanaryTarget::StrictSsh(target),
        Sha256Digest::of_bytes(uuid().as_bytes()),
        staging,
    )
    .unwrap();
    let runner = FakeRunner {
        outputs: VecDeque::from([Ok(output(staging))]),
        ..FakeRunner::default()
    };
    let mut observer = StrictKnownHostCanaryObserver::with_runner(runner);
    let receipt = observer.observe(&spec, Duration::from_secs(2)).unwrap();
    assert_eq!(receipt.host_id(), "m1");
    assert_eq!(receipt.observed_staging_root(), Some(staging));
    assert_eq!(receipt.free_bytes(), Some(2_048_000_000));
    assert_eq!(receipt.model_calls(), 0);

    let argv = &observer.runner.argv[0];
    let rendered = argv
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>();
    assert!(
        rendered
            .iter()
            .any(|arg| *arg == "StrictHostKeyChecking=yes")
    );
    assert!(rendered.iter().any(|arg| *arg == "UpdateHostKeys=no"));
    assert!(
        rendered
            .iter()
            .any(|arg| *arg == "UserKnownHostsFile=/dev/null")
    );
    assert!(
        rendered.iter().any(|arg| {
            *arg == "KnownHostsCommand=/usr/bin/printenv SHIPYARD_CANARY_KNOWN_HOSTS"
        })
    );
    assert!(rendered.windows(2).any(|args| args == ["-F", "/dev/null"]));
    for expected in [
        "IdentitiesOnly=yes",
        "ControlMaster=no",
        "ControlPath=none",
        "ProxyCommand=none",
        "ProxyJump=none",
        "PermitLocalCommand=no",
        "ClearAllForwardings=yes",
    ] {
        assert!(rendered.iter().any(|arg| *arg == expected));
    }
    assert_eq!(
        observer.runner.known_host_authorities,
        ["m1-lan ssh-ed25519 AAAATEST\n"]
    );
    assert!(!rendered.iter().any(|arg| arg.contains("accept-new")));
    assert!(!rendered.iter().any(|arg| arg == "--execute"));
}

#[cfg(not(unix))]
#[test]
fn strict_ssh_observer_refuses_without_unix_authority() {
    let temp = TempDir::new().unwrap();
    let known_hosts = temp.path().join("known_hosts");
    let identity = temp.path().join("identity");
    fs::write(&known_hosts, "m1-lan ssh-ed25519 AAAATEST\n").unwrap();
    fs::write(&identity, "private-test-key").unwrap();
    let target = StrictSshCanaryTarget::new(
        std::env::current_exe().expect("current executable"),
        "m1-lan",
        &known_hosts,
        &identity,
        22,
    )
    .unwrap();
    let result = target.prepare_remote_command("/usr/bin/true", &[]);
    let Err(error) = result else {
        panic!("non-Unix strict SSH authority must fail closed");
    };
    assert!(matches!(
        error,
        CanaryObserverError::InvalidConfiguration(_)
    ));
}

#[test]
fn identity_drift_and_symlinked_known_hosts_fail_closed() {
    let staging = "/Users/test/shipyard-canary";
    let runner = FakeRunner {
        outputs: VecDeque::from([Ok(output(staging))]),
        ..FakeRunner::default()
    };
    let mut observer = StrictKnownHostCanaryObserver::with_runner(runner);
    let mut spec = local_spec(staging);
    spec.expected_platform_identity_sha256 = Sha256Digest::of_bytes(b"different");
    assert!(matches!(
        observer.observe(&spec, Duration::from_secs(1)),
        Err(CanaryObserverError::IdentityMismatch)
    ));

    let temp = TempDir::new().unwrap();
    let authority = temp.path().join("authority");
    let identity = temp.path().join("identity");
    fs::write(&authority, "host ssh-ed25519 key\n").unwrap();
    fs::write(&identity, "private-test-key").unwrap();
    #[cfg(unix)]
    {
        let link = temp.path().join("known_hosts");
        std::os::unix::fs::symlink(&authority, &link).unwrap();
        let target =
            StrictSshCanaryTarget::new("/usr/bin/ssh", "m1", &link, &identity, 22).unwrap();
        let spec = ReadOnlyCanaryHostSpec::new(
            "m1",
            ReadOnlyCanaryTarget::StrictSsh(target),
            Sha256Digest::of_bytes(uuid().as_bytes()),
            staging,
        )
        .unwrap();
        let mut observer = StrictKnownHostCanaryObserver::with_runner(FakeRunner::default());
        assert!(matches!(
            observer.observe(&spec, Duration::from_secs(1)),
            Err(CanaryObserverError::AuthorityUnreadable(_))
        ));
    }
}

#[cfg(unix)]
#[test]
fn non_utf8_staging_path_is_rejected_before_observation() {
    use std::os::unix::ffi::OsStringExt;

    let invalid = PathBuf::from(OsString::from_vec(
        b"/Users/test/shipyard-canary-\xff".to_vec(),
    ));
    assert!(matches!(
        ReadOnlyCanaryHostSpec::new(
            "m3",
            ReadOnlyCanaryTarget::Local,
            Sha256Digest::of_bytes(uuid().as_bytes()),
            invalid,
        ),
        Err(CanaryObserverError::InvalidConfiguration(_))
    ));
}

#[test]
fn parser_rejects_duplicate_unknown_and_overflowed_observations() {
    let duplicate = format!(
        "schema\t1\nplatform_uuid\t{}\nplatform_uuid\t{}\nboot_seconds\t1\nstaging\tmissing\n",
        uuid(),
        uuid()
    );
    assert!(parse_probe_output(duplicate.as_bytes()).is_err());
    let unknown = format!(
        "schema\t1\nplatform_uuid\t{}\nboot_seconds\t1\nstaging\tmissing\nclaim\ttrusted\n",
        uuid()
    );
    assert!(parse_probe_output(unknown.as_bytes()).is_err());
    let overflow = format!(
        "schema\t1\nplatform_uuid\t{}\nboot_seconds\t1\nstaging\tpresent\ncanonical_root\t/Users/test/canary\nfree_kib\t{}\n",
        uuid(),
        u64::MAX
    );
    assert!(parse_probe_output(overflow.as_bytes()).is_err());
}

#[test]
fn dry_run_is_ineligible_and_never_synthesizes_missing_proofs() {
    let m3_staging = "/Users/test/m3-canary";
    let m1_staging = "/Users/test/m1-canary";
    let m3_receipt = observed_host("m3", m3_staging);
    let m1_receipt = observed_host("m1", m1_staging);
    let policy = PulpMacCanaryPolicy {
        enabled: true,
        assessed_at_ms: controller_now_ms().unwrap(),
        minimum_free_bytes: 1024,
        ..pulp_policy()
    };
    let readiness =
        classify_pulp_mac_dry_run_readiness(&policy, 4096, &[m3_receipt, m1_receipt]).unwrap();
    assert!(matches!(
        readiness.decision(),
        PulpMacCanaryDecision::Ineligible { reasons }
            if reasons.contains(&CanaryIneligibleReason::SessionGenerationMissing)
                && reasons.contains(&CanaryIneligibleReason::RouteIneligible)
                && reasons.contains(&CanaryIneligibleReason::CacheGenerationMismatch)
                && reasons.contains(&CanaryIneligibleReason::CapabilityMismatch)
    ));
    assert!(readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::LanRouteAuthorityMissing { host_id }
            if host_id == "m1"
    )));
    assert_eq!(readiness.model_calls(), 0);
    assert!(!readiness.would_mutate());
}

#[test]
fn exact_cache_evidence_closes_only_the_cache_gap() {
    let cache_root = tempfile::Builder::new()
        .prefix(".shipyard-controller-cache-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    fs::write(cache_root.path().join("object.bin"), b"cache-object").unwrap();
    let manifest = produce_cache_generation_manifest(cache_root.path(), "skia", "m124").unwrap();

    let m3_staging = "/Users/test/m3-canary";
    let m1_staging = "/Users/test/m1-canary";
    let m3_receipt = observed_host("m3", m3_staging);
    let m1_receipt = observed_host("m1", m1_staging);
    let assessed_at_ms = controller_now_ms().unwrap();
    let cache_receipt = |host_id: &str, host_observation_sha256: Sha256Digest| {
        let remote_authority = (host_id == "m1").then(|| {
            remote_cache_authority(
                host_observation_sha256.clone(),
                cache_root.path(),
                &manifest,
                m1_staging,
                assessed_at_ms,
            )
        });
        CacheGenerationObservationReceipt {
            schema_version: CACHE_GENERATION_OBSERVATION_SCHEMA,
            host_id: host_id.to_owned(),
            host_observation_sha256: host_observation_sha256.clone(),
            observed_at_ms: assessed_at_ms,
            probe_elapsed_ms: 1,
            cache_root: cache_root.path().to_str().unwrap().to_owned(),
            manifest_sha256: manifest.digest().unwrap(),
            manifest: manifest.clone(),
            remote_authority,
            model_calls: 0,
        }
    };
    let m3_digest = m3_receipt.digest().unwrap();
    let m1_digest = m1_receipt.digest().unwrap();
    let cache_evidence = cache_evidence(
        assessed_at_ms,
        vec![cache_receipt("m3", m3_digest)],
        vec![cache_receipt("m1", m1_digest)],
    );
    let policy = PulpMacCanaryPolicy {
        enabled: true,
        assessed_at_ms,
        minimum_free_bytes: 1024,
        required_cache_generations: vec![manifest.generation],
        ..pulp_policy()
    };
    let host_receipts = [m3_receipt, m1_receipt];
    let readiness = classify_pulp_mac_dry_run_readiness_with_cache(
        &policy,
        4096,
        &host_receipts,
        Some(&cache_evidence),
    )
    .unwrap();
    assert!(matches!(
        readiness.decision(),
        PulpMacCanaryDecision::Ineligible { reasons }
            if !reasons.contains(&CanaryIneligibleReason::CacheGenerationMismatch)
                && reasons.contains(&CanaryIneligibleReason::SessionGenerationMissing)
                && !reasons.contains(&CanaryIneligibleReason::RouteIneligible)
                && reasons.contains(&CanaryIneligibleReason::CapabilityMismatch)
    ));
    assert!(!readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::CacheGenerationAuthorityMissing { .. }
    )));
    assert!(!readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::SessionGenerationAuthorityMissing { host_id }
            | PhysicalCanaryReadinessGap::LanRouteAuthorityMissing { host_id }
            if host_id == "m1"
    )));
    assert!(readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::CapabilityAuthorityMissing { host_id }
            if host_id == "m1"
    )));

    let mut detached_cache_evidence = cache_evidence;
    detached_cache_evidence.builder[0].host_observation_sha256 =
        Sha256Digest::of_bytes(b"different-host-observation");
    let detached = classify_pulp_mac_dry_run_readiness_with_cache(
        &policy,
        4096,
        &host_receipts,
        Some(&detached_cache_evidence),
    )
    .unwrap();
    assert!(matches!(
        detached.decision(),
        PulpMacCanaryDecision::Ineligible { reasons }
            if reasons.contains(&CanaryIneligibleReason::CacheGenerationMismatch)
    ));
}

#[test]
fn stale_and_future_receipts_cannot_supply_storage_readiness() {
    let staging = "/Users/test/m3-canary";
    let mut observer = StrictKnownHostCanaryObserver::with_runner(FakeRunner {
        outputs: VecDeque::from([Ok(output(staging))]),
        ..FakeRunner::default()
    });
    let mut receipt = observer
        .observe(&local_spec(staging), Duration::from_secs(1))
        .unwrap();
    let policy = PulpMacCanaryPolicy {
        assessed_at_ms: receipt.observed_at_ms,
        maximum_observation_age_ms: 1,
        ..pulp_policy()
    };
    receipt.observed_at_ms = receipt.observed_at_ms.saturating_add(1);
    let readiness = classify_pulp_mac_dry_run_readiness(&policy, 1024, &[receipt]).unwrap();
    assert!(readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::HostObservationStale { host_id }
            if host_id == "m3"
    )));
    assert!(readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::StorageReserveUnproven { host_id }
            if host_id == "m3"
    )));
}

#[test]
fn absent_staging_and_duplicate_hosts_remain_ineligible() {
    let staging = "/Users/test/m3-canary";
    let missing = ReadOnlyProbeOutput::new(
        true,
        format!(
            "schema\t1\nplatform_uuid\t{}\nboot_seconds\t1\nstaging\tmissing\n",
            uuid()
        )
        .into_bytes(),
        Vec::new(),
    );
    let mut observer = StrictKnownHostCanaryObserver::with_runner(FakeRunner {
        outputs: VecDeque::from([Ok(missing)]),
        ..FakeRunner::default()
    });
    let receipt = observer
        .observe(&local_spec(staging), Duration::from_secs(1))
        .unwrap();
    let readiness =
        classify_pulp_mac_dry_run_readiness(&pulp_policy(), 1024, &[receipt.clone(), receipt])
            .unwrap();
    assert!(readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::HostObservationMissing { host_id }
            if host_id == "m3"
    )));
    assert!(readiness.gaps().iter().any(|gap| matches!(
        gap,
        PhysicalCanaryReadinessGap::HostObservationMissing { host_id }
            if host_id == "m1"
    )));
}
