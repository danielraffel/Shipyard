use std::collections::VecDeque;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(unix)]
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::*;
use crate::parallel_proof_canary::PulpMacCanaryPolicy;
use crate::parallel_proof_canary_cache::{
    PulpMacCacheEvidenceStore, PulpMacCacheProbeRequest, drive_pulp_mac_cache_probe,
};

fn persistent_temp() -> TempDir {
    let current = std::env::current_dir().unwrap().canonicalize().unwrap();
    tempfile::Builder::new()
        .prefix(".shipyard-remote-cache-test-")
        .tempdir_in(current)
        .unwrap()
}

fn cache_tree() -> TempDir {
    let root = persistent_temp();
    fs::create_dir(root.path().join("objects")).unwrap();
    fs::write(root.path().join("objects/cache.bin"), b"immutable-cache").unwrap();
    root
}

fn digest(label: &str) -> Sha256Digest {
    Sha256Digest::of_bytes(label.as_bytes())
}

fn authority(host_observation_sha256: Sha256Digest) -> RemoteM1CacheAuthority {
    RemoteM1CacheAuthority {
        source_host_id: "m3".to_owned(),
        host_id: "m1".to_owned(),
        host_observation_sha256,
        host_session_generation: 12,
        route: CanaryRoute::Lan,
        destination: "shipyard@m1.local".to_owned(),
        known_hosts_sha256: digest("known-hosts"),
        capabilities: vec!["macos-arm64".to_owned()],
        staging_root: "/Users/test/shipyard-staging".to_owned(),
        staging_class: CanaryStagingClass::Persistent,
        free_bytes: 10_000,
        artifact_bytes_total: 1_000,
        minimum_reserve_bytes: 1_000,
        terminal_instance_sha256: digest("verified-terminal-instance"),
        companion_executable_sha256: digest("paired-companion"),
        observed_at_ms: controller_now_ms().unwrap(),
        model_calls: 0,
    }
}

#[cfg(unix)]
#[derive(Clone, Debug)]
enum FakeCommandStep {
    Success { stdout: Vec<u8>, elapsed_ms: u64 },
    Companion { elapsed_ms: u64 },
    Failure(RemoteM1CacheCarrierFailureClass),
}

#[cfg(unix)]
#[derive(Clone, Debug)]
struct RecordedCommand {
    arguments: Vec<String>,
    environment: Vec<(String, Option<String>)>,
    request: Vec<u8>,
}

#[cfg(unix)]
#[derive(Clone)]
struct FakeCommandRunner {
    steps: Arc<Mutex<VecDeque<FakeCommandStep>>>,
    calls: Arc<Mutex<Vec<RecordedCommand>>>,
}

#[cfg(unix)]
impl FakeCommandRunner {
    fn new(steps: impl IntoIterator<Item = FakeCommandStep>) -> Self {
        Self {
            steps: Arc::new(Mutex::new(steps.into_iter().collect())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[cfg(unix)]
impl RemoteM1CacheCommandRunner for FakeCommandRunner {
    fn run(
        &mut self,
        command: &mut Command,
        request: &[u8],
        _deadline: Instant,
        _maximum_stdout_bytes: u64,
    ) -> Result<RemoteM1CacheCommandOutput, RemoteM1CacheCarrierFailureClass> {
        self.calls.lock().unwrap().push(RecordedCommand {
            arguments: command
                .get_args()
                .map(|value| value.to_string_lossy().into_owned())
                .collect(),
            environment: command
                .get_envs()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect(),
            request: request.to_vec(),
        });
        match self.steps.lock().unwrap().pop_front().unwrap() {
            FakeCommandStep::Success { stdout, elapsed_ms } => Ok(RemoteM1CacheCommandOutput {
                status: ExitStatus::from_raw(0),
                stdout,
                elapsed_ms,
            }),
            FakeCommandStep::Companion { elapsed_ms } => {
                let request: RemoteM1CacheRequest = serde_json::from_slice(request).unwrap();
                let response = handle_remote_m1_cache_request(&request, |_| Ok(())).unwrap();
                Ok(RemoteM1CacheCommandOutput {
                    status: ExitStatus::from_raw(0),
                    stdout: serde_json::to_vec(&response).unwrap(),
                    elapsed_ms,
                })
            }
            FakeCommandStep::Failure(class) => Err(class),
        }
    }
}

#[cfg(unix)]
fn strict_target(root: &TempDir, label: &str, destination: &str) -> StrictSshCanaryTarget {
    let known_hosts = root.path().join(format!("{label}-known-hosts"));
    let identity = root.path().join(format!("{label}-identity"));
    fs::write(
        &known_hosts,
        format!("{destination} ssh-ed25519 test-key\n"),
    )
    .unwrap();
    fs::write(&identity, b"test-private-key").unwrap();
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
    StrictSshCanaryTarget::new("/usr/bin/ssh", destination, known_hosts, identity, 22).unwrap()
}

struct FakeTransport {
    authorities: VecDeque<RemoteM1CacheAuthority>,
    calls: Vec<&'static str>,
    tamper_stats: bool,
    remote_clock_ms: Option<u64>,
}

impl RemoteM1CacheTransport for FakeTransport {
    fn authenticate_m1(
        &mut self,
        _deadline: Instant,
    ) -> Result<RemoteM1CacheAuthority, CacheObserverError> {
        self.calls.push("authenticate");
        self.authorities
            .pop_front()
            .ok_or_else(|| CacheObserverError::Invalid("missing fake authority".to_owned()))
    }

    fn invoke_cache_observer(
        &mut self,
        request_bytes: &[u8],
        _deadline: Instant,
    ) -> Result<RemoteM1CacheTransportOutput, CacheObserverError> {
        self.calls.push("invoke");
        let request: RemoteM1CacheRequest = serde_json::from_slice(request_bytes)?;
        let mut response = handle_remote_m1_cache_request(&request, |_| Ok(()))
            .map_err(CacheObserverError::Invalid)?;
        if let Some(remote_clock_ms) = self.remote_clock_ms {
            response.observed_at_ms = remote_clock_ms;
        }
        let response = serde_json::to_vec(&response)?;
        Ok(RemoteM1CacheTransportOutput {
            stats: RemoteM1CacheTransportStats {
                route: CanaryRoute::Lan,
                lan_probe_round_trip_ms: Some(1),
                tailnet_probe_round_trip_ms: None,
                fallback_class: None,
                request_sha256: if self.tamper_stats {
                    digest("wrong-request")
                } else {
                    Sha256Digest::of_bytes(request_bytes)
                },
                response_sha256: Sha256Digest::of_bytes(&response),
                request_bytes_sent: request_bytes.len() as u64,
                response_bytes_received: response.len() as u64,
                round_trip_ms: 1,
            },
            response,
        })
    }
}

#[test]
fn remote_observer_binds_every_authority_and_transport_counter() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let host_digest = digest("authenticated-m1-host-observation");
    let spec =
        CacheGenerationProbeSpec::new("m1", host_digest.clone(), root.path(), manifest.clone())
            .unwrap();
    let transport = FakeTransport {
        authorities: VecDeque::from([authority(host_digest.clone())]),
        calls: Vec::new(),
        tamper_stats: false,
        remote_clock_ms: None,
    };
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        transport,
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    let receipt = observer.observe(&spec).unwrap();
    assert_eq!(receipt.host_id, "m1");
    assert_eq!(receipt.manifest, manifest);
    let remote = receipt.remote_authority.as_ref().unwrap();
    assert_eq!(remote.authority.host_session_generation, 12);
    assert_eq!(remote.authority.route, CanaryRoute::Lan);
    assert_eq!(remote.authority.host_observation_sha256, host_digest);
    assert_eq!(remote.model_calls, 0);
    assert_eq!(observer.transport.calls, ["authenticate", "invoke"]);

    let mut corrupted = receipt;
    corrupted
        .remote_authority
        .as_mut()
        .unwrap()
        .transport
        .response_bytes_received += 1;
    assert!(corrupted.validate().is_err());
}

#[test]
fn remote_observer_uses_explicit_non_pulp_host_pair() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let host_digest = digest("worker-b-observation");
    let spec =
        CacheGenerationProbeSpec::new("worker-b", host_digest.clone(), root.path(), manifest)
            .unwrap();
    let mut observed_authority = authority(host_digest);
    observed_authority.source_host_id = "builder-a".to_owned();
    observed_authority.host_id = "worker-b".to_owned();
    let transport = FakeTransport {
        authorities: VecDeque::from([observed_authority]),
        calls: Vec::new(),
        tamper_stats: false,
        remote_clock_ms: None,
    };
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        transport,
        "builder-a",
        "worker-b",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    let receipt = observer.observe(&spec).unwrap();
    assert_eq!(receipt.host_id, "worker-b");
    assert_eq!(
        receipt.remote_authority.unwrap().authority.source_host_id,
        "builder-a"
    );
}

#[test]
fn paired_observer_rejects_a_remote_source_other_than_its_builder() {
    let remote = AuthenticatedRemoteM1CacheObserver::new(
        FakeTransport {
            authorities: VecDeque::new(),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: None,
        },
        "other-builder",
        "worker-b",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    assert!(matches!(
        PairedAuthenticatedCacheObserver::new("builder-a", "worker-b", remote),
        Err(CacheObserverError::Invalid(message))
            if message == "paired cache observer host binding"
    ));
}

#[test]
fn remote_wall_clock_is_authenticated_but_never_used_for_controller_freshness() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let host_digest = digest("authenticated-m1-host-observation");
    let spec =
        CacheGenerationProbeSpec::new("m1", host_digest.clone(), root.path(), manifest).unwrap();
    let authority = authority(host_digest);
    let authority_time = authority.observed_at_ms;
    let remote_clock_ms = u64::MAX - 1;
    let transport = FakeTransport {
        authorities: VecDeque::from([authority]),
        calls: Vec::new(),
        tamper_stats: false,
        remote_clock_ms: Some(remote_clock_ms),
    };
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        transport,
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    let receipt = observer.observe(&spec).unwrap();
    assert!(receipt.observed_at_ms >= authority_time);
    assert_ne!(receipt.observed_at_ms, remote_clock_ms);
    assert_eq!(
        receipt
            .remote_authority
            .as_ref()
            .unwrap()
            .remote_observed_at_ms,
        remote_clock_ms
    );
    assert!(receipt.validate().is_ok());
}

#[test]
fn companion_digest_is_verified_before_cache_observation() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let authority = authority(digest("authenticated-m1-host-observation"));
    let request =
        RemoteM1CacheRequest::from_parts(root.path().to_str().unwrap(), &manifest, &authority)
            .unwrap();
    fs::remove_file(root.path().join("objects/cache.bin")).unwrap();

    let error =
        handle_remote_m1_cache_request(&request, |_| Err("companion digest refused".to_owned()))
            .unwrap_err();
    assert_eq!(error, "companion digest refused");
}

#[test]
fn detached_authority_and_tampered_transport_fail_closed() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let spec =
        CacheGenerationProbeSpec::new("m1", digest("expected-host"), root.path(), manifest.clone())
            .unwrap();
    let detached = FakeTransport {
        authorities: VecDeque::from([authority(digest("other-host"))]),
        calls: Vec::new(),
        tamper_stats: false,
        remote_clock_ms: None,
    };
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        detached,
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    assert!(observer.observe(&spec).is_err());
    assert_eq!(observer.transport.calls, ["authenticate"]);

    let tampered = FakeTransport {
        authorities: VecDeque::from([authority(digest("expected-host"))]),
        calls: Vec::new(),
        tamper_stats: true,
        remote_clock_ms: None,
    };
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        tampered,
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    assert!(observer.observe(&spec).is_err());
    assert_eq!(observer.transport.calls, ["authenticate", "invoke"]);
}

#[test]
fn non_lan_or_insufficient_reserve_authority_is_refused_before_invocation() {
    let root = cache_tree();
    let manifest = produce_cache_generation_manifest(root.path(), "skia", "m124").unwrap();
    let host_digest = digest("expected-host");
    let spec =
        CacheGenerationProbeSpec::new("m1", host_digest.clone(), root.path(), manifest).unwrap();
    let mut invalid = authority(host_digest);
    invalid.route = CanaryRoute::Tailnet;
    invalid.free_bytes = 1;
    let transport = FakeTransport {
        authorities: VecDeque::from([invalid]),
        calls: Vec::new(),
        tamper_stats: false,
        remote_clock_ms: None,
    };
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        transport,
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    assert!(observer.observe(&spec).is_err());
    assert_eq!(observer.transport.calls, ["authenticate"]);
}

#[test]
fn paired_driver_never_reaches_m1_after_failed_local_m3_proof() {
    let m3_root = cache_tree();
    let m1_root = cache_tree();
    let expected = produce_cache_generation_manifest(m1_root.path(), "skia", "m124").unwrap();
    fs::write(m3_root.path().join("objects/cache.bin"), b"different-cache").unwrap();
    let m3_digest = digest("m3-host");
    let m1_digest = digest("m1-host");
    let request = PulpMacCacheProbeRequest {
        enabled: true,
        correlation_id: "paired-m3-first".to_owned(),
        builder: vec![
            CacheGenerationProbeSpec::new("m3", m3_digest, m3_root.path(), expected.clone())
                .unwrap(),
        ],
        worker: vec![
            CacheGenerationProbeSpec::new(
                "m1",
                m1_digest.clone(),
                m1_root.path(),
                expected.clone(),
            )
            .unwrap(),
        ],
    };
    let policy = PulpMacCanaryPolicy {
        enabled: true,
        repository_id: 1_203_111_607,
        repository: "generous-corp/pulp".to_owned(),
        target: "mac".to_owned(),
        target_triple: "aarch64-apple-darwin".to_owned(),
        builder_host_id: "m3".to_owned(),
        worker_host_id: "m1".to_owned(),
        assessed_at_ms: controller_now_ms().unwrap(),
        required_cache_generations: vec![expected.generation.clone()],
        ..PulpMacCanaryPolicy::default()
    };
    let remote = AuthenticatedRemoteM1CacheObserver::new(
        FakeTransport {
            authorities: VecDeque::from([authority(m1_digest)]),
            calls: Vec::new(),
            tamper_stats: false,
            remote_clock_ms: None,
        },
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    let mut observer = PairedAuthenticatedCacheObserver::new("m3", "m1", remote).unwrap();
    let store_parent = persistent_temp();
    let store = PulpMacCacheEvidenceStore::open(store_parent.path().join("evidence")).unwrap();
    assert!(drive_pulp_mac_cache_probe(&request, &policy, &mut observer, &store).is_err());
    assert!(observer.remote.transport.calls.is_empty());
}

#[cfg(unix)]
#[test]
fn production_carrier_uses_pinned_lan_and_stdin_without_ambient_ssh() {
    let cache = cache_tree();
    let manifest = produce_cache_generation_manifest(cache.path(), "skia", "m124").unwrap();
    let host = digest("m1-host");
    let spec = CacheGenerationProbeSpec::new("m1", host.clone(), cache.path(), manifest).unwrap();
    let authorities = persistent_temp();
    let runner = FakeCommandRunner::new([
        FakeCommandStep::Success {
            stdout: Vec::new(),
            elapsed_ms: 3,
        },
        FakeCommandStep::Companion { elapsed_ms: 7 },
    ]);
    let calls = Arc::clone(&runner.calls);
    let transport = StrictSshRemoteM1CacheTransport::with_runner(
        strict_target(&authorities, "lan", "shipyard@m1.local"),
        None,
        authority(host),
        "/usr/local/bin/shipyard-workstream-provider",
        Duration::from_secs(1),
        runner,
    )
    .unwrap();
    let mut observer = AuthenticatedRemoteM1CacheObserver::new(
        transport,
        "m3",
        "m1",
        Duration::from_secs(1),
        60_000,
    )
    .unwrap();
    let receipt = observer.observe(&spec).unwrap();
    let remote = receipt.remote_authority.unwrap();
    assert_eq!(remote.authority.route, CanaryRoute::Lan);
    assert_eq!(remote.transport.lan_probe_round_trip_ms, Some(3));
    assert_eq!(remote.transport.tailnet_probe_round_trip_ms, None);
    assert_eq!(remote.transport.fallback_class, None);
    assert_eq!(remote.transport.round_trip_ms, 7);
    assert_eq!(remote.model_calls, 0);

    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert!(calls[0].request.is_empty());
    assert!(!calls[1].request.is_empty());
    assert!(calls[1].arguments.windows(2).any(|pair| {
        pair == [
            "/usr/local/bin/shipyard-workstream-provider",
            "--observe-m1-cache",
        ]
    }));
    assert!(
        !calls[1]
            .arguments
            .iter()
            .any(|argument| argument.contains("expected_manifest"))
    );
    assert!(
        calls[1]
            .arguments
            .windows(2)
            .any(|pair| pair == ["-F", "/dev/null"])
    );
    assert!(
        calls[1]
            .arguments
            .iter()
            .any(|argument| argument == "IdentityAgent=none")
    );
    assert_eq!(
        calls[1]
            .environment
            .iter()
            .map(|(key, _)| key.as_str())
            .collect::<Vec<_>>(),
        ["LANG", "LC_ALL", "SHIPYARD_CANARY_KNOWN_HOSTS"]
    );
}

#[cfg(unix)]
#[test]
fn production_carrier_measures_tailnet_fallback_without_redispatching_request() {
    let authorities = persistent_temp();
    let runner = FakeCommandRunner::new([
        FakeCommandStep::Failure(RemoteM1CacheCarrierFailureClass::Unavailable),
        FakeCommandStep::Success {
            stdout: Vec::new(),
            elapsed_ms: 5,
        },
        FakeCommandStep::Success {
            stdout: b"bounded-response".to_vec(),
            elapsed_ms: 11,
        },
    ]);
    let calls = Arc::clone(&runner.calls);
    let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
        strict_target(&authorities, "lan", "shipyard@m1.local"),
        Some(strict_target(
            &authorities,
            "tailnet",
            "shipyard@m1.tailnet",
        )),
        authority(digest("m1-host")),
        "/usr/local/bin/shipyard-workstream-provider",
        Duration::from_secs(1),
        runner,
    )
    .unwrap();
    let selected = transport
        .authenticate_m1(Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert_eq!(selected.route, CanaryRoute::Tailnet);
    assert_eq!(selected.destination, "shipyard@m1.tailnet");
    let request = b"bounded-request";
    let output = transport
        .invoke_cache_observer(request, Instant::now() + Duration::from_secs(1))
        .unwrap();
    assert_eq!(output.stats.route, CanaryRoute::Tailnet);
    assert!(output.stats.lan_probe_round_trip_ms.is_some());
    assert_eq!(output.stats.tailnet_probe_round_trip_ms, Some(5));
    assert_eq!(
        output.stats.fallback_class,
        Some(RemoteM1CacheCarrierFailureClass::Unavailable)
    );
    assert_eq!(output.stats.round_trip_ms, 11);
    let calls = calls.lock().unwrap();
    assert_eq!(
        calls.iter().filter(|call| !call.request.is_empty()).count(),
        1
    );
    assert_eq!(calls[2].request, request);
    assert!(
        calls[2]
            .arguments
            .iter()
            .any(|argument| argument == "shipyard@m1.tailnet")
    );
}

#[cfg(unix)]
#[test]
fn interrupted_companion_transfer_is_classified_and_never_retried() {
    let authorities = persistent_temp();
    let runner = FakeCommandRunner::new([
        FakeCommandStep::Success {
            stdout: Vec::new(),
            elapsed_ms: 2,
        },
        FakeCommandStep::Failure(RemoteM1CacheCarrierFailureClass::Interrupted),
    ]);
    let calls = Arc::clone(&runner.calls);
    let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
        strict_target(&authorities, "lan", "shipyard@m1.local"),
        Some(strict_target(
            &authorities,
            "tailnet",
            "shipyard@m1.tailnet",
        )),
        authority(digest("m1-host")),
        "/usr/local/bin/shipyard-workstream-provider",
        Duration::from_secs(1),
        runner,
    )
    .unwrap();
    transport
        .authenticate_m1(Instant::now() + Duration::from_secs(1))
        .unwrap();
    let error = transport
        .invoke_cache_observer(
            b"request-in-flight",
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "remote M1 cache carrier interrupted");
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].request, b"request-in-flight");
}

#[cfg(unix)]
#[test]
fn local_ssh_authority_failure_never_falls_back_to_tailnet() {
    let authorities = persistent_temp();
    let lan = strict_target(&authorities, "lan", "shipyard@m1.local");
    fs::set_permissions(
        authorities.path().join("lan-identity"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let runner = FakeCommandRunner::new([FakeCommandStep::Success {
        stdout: Vec::new(),
        elapsed_ms: 1,
    }]);
    let calls = Arc::clone(&runner.calls);
    let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
        lan,
        Some(strict_target(
            &authorities,
            "tailnet",
            "shipyard@m1.tailnet",
        )),
        authority(digest("m1-host")),
        "/usr/local/bin/shipyard-workstream-provider",
        Duration::from_secs(1),
        runner,
    )
    .unwrap();
    let error = transport
        .authenticate_m1(Instant::now() + Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(error.to_string(), "remote M1 cache carrier remote_refused");
    assert!(calls.lock().unwrap().is_empty());
}

#[test]
fn route_measurement_shapes_fail_closed() {
    let baseline = RemoteM1CacheTransportStats {
        route: CanaryRoute::Lan,
        lan_probe_round_trip_ms: Some(1),
        tailnet_probe_round_trip_ms: None,
        fallback_class: None,
        request_sha256: digest("request"),
        response_sha256: digest("response"),
        request_bytes_sent: 1,
        response_bytes_received: 1,
        round_trip_ms: 1,
    };
    assert!(valid_route_measurements(&baseline));
    let mut invalid_lan = baseline.clone();
    invalid_lan.fallback_class = Some(RemoteM1CacheCarrierFailureClass::Unavailable);
    assert!(!valid_route_measurements(&invalid_lan));
    let mut invalid_tailnet = baseline.clone();
    invalid_tailnet.route = CanaryRoute::Tailnet;
    invalid_tailnet.tailnet_probe_round_trip_ms = Some(2);
    invalid_tailnet.fallback_class = Some(RemoteM1CacheCarrierFailureClass::RemoteRefused);
    assert!(!valid_route_measurements(&invalid_tailnet));
    invalid_tailnet.fallback_class = Some(RemoteM1CacheCarrierFailureClass::TimedOut);
    invalid_tailnet.lan_probe_round_trip_ms = None;
    assert!(!valid_route_measurements(&invalid_tailnet));
}

#[cfg(unix)]
#[test]
fn oversized_request_is_refused_before_companion_invocation() {
    let authorities = persistent_temp();
    let runner = FakeCommandRunner::new([FakeCommandStep::Success {
        stdout: Vec::new(),
        elapsed_ms: 1,
    }]);
    let calls = Arc::clone(&runner.calls);
    let mut transport = StrictSshRemoteM1CacheTransport::with_runner(
        strict_target(&authorities, "lan", "shipyard@m1.local"),
        None,
        authority(digest("m1-host")),
        "/usr/local/bin/shipyard-workstream-provider",
        Duration::from_secs(1),
        runner,
    )
    .unwrap();
    transport
        .authenticate_m1(Instant::now() + Duration::from_secs(1))
        .unwrap();
    let oversized = vec![0_u8; usize::try_from(MAX_COMPANION_MESSAGE_BYTES + 1).unwrap()];
    let error = transport
        .invoke_cache_observer(&oversized, Instant::now() + Duration::from_secs(1))
        .unwrap_err();
    assert_eq!(error.to_string(), "remote M1 cache carrier output_limit");
    assert_eq!(calls.lock().unwrap().len(), 1);
}
