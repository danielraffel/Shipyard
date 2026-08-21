use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use clap::Parser;

use super::*;
use crate::app::cli::Cli;

fn identity(version: &str, byte: char) -> ReleaseIdentity {
    ReleaseIdentity::new(version, &byte.to_string().repeat(64)).expect("identity")
}

fn host(name: &str, local: bool, canary: bool) -> FleetHost {
    FleetHost {
        name: name.to_owned(),
        ssh: (!local).then(|| name.to_ascii_lowercase()),
        local,
        canary,
    }
}

fn observation(
    desired: &ReleaseIdentity,
    reachable: bool,
    busy: bool,
    converged: bool,
) -> HostObservation {
    HostObservation {
        reachable,
        version: reachable.then(|| {
            if converged {
                desired.version.clone()
            } else {
                "0.1.0".to_owned()
            }
        }),
        sha256: reachable.then(|| {
            if converged {
                desired.sha256.clone()
            } else {
                "0".repeat(64)
            }
        }),
        daemon_running: reachable.then_some(true),
        daemon_version: reachable.then(|| {
            if converged {
                desired.version.clone()
            } else {
                "0.1.0".to_owned()
            }
        }),
        participation: reachable.then_some(true),
        drain_owned: reachable.then_some(false),
        busy: reachable.then_some(busy),
        detail: "fixture".to_owned(),
    }
}

#[test]
fn immutable_identity_rejects_version_only_or_non_digest_input() {
    assert!(ReleaseIdentity::new("v0.97.0", &"a".repeat(64)).is_ok());
    assert!(ReleaseIdentity::new("latest", &"a".repeat(64)).is_err());
    assert!(ReleaseIdentity::new("1", &"a".repeat(64)).is_err());
    assert!(ReleaseIdentity::new("1.2", &"a".repeat(64)).is_err());
    assert!(ReleaseIdentity::new("1.2.3.4", &"a".repeat(64)).is_err());
    assert!(ReleaseIdentity::new("vv1.2.3", &"a".repeat(64)).is_err());
    assert!(ReleaseIdentity::new("0.97.0", "release-dmg-sha").is_err());
}

#[test]
fn apply_cli_requires_both_exact_rollback_fields() {
    let args = [
        "shipyard",
        "fleet",
        "release",
        "apply",
        "--to",
        "v0.97.0",
        "--sha256",
        &"a".repeat(64),
    ];
    assert!(Cli::try_parse_from(args).is_err());
}

#[test]
fn offline_declared_canary_does_not_serialize_an_eligible_peer() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![
        host("m1", false, true),
        host("m3", true, false),
        host("m5", false, false),
    ];
    let now = timestamp();
    let receipts = receipt_map(
        &hosts,
        vec![
            ("m1".to_owned(), observation(&desired, false, false, false)),
            ("m3".to_owned(), observation(&desired, true, false, false)),
            ("m5".to_owned(), observation(&desired, true, false, false)),
        ],
        &desired,
        &now,
    );

    let canary = select_canary(&hosts, &receipts, None, false);
    assert_eq!(canary, "m3");
    assert_eq!(next_wave(&hosts, &receipts, &canary, false), vec!["m3"]);
}

#[test]
fn after_canary_all_eligible_hosts_share_one_wave_while_offline_stays_pending() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![
        host("m3", true, true),
        host("m1", false, false),
        host("m5", false, false),
    ];
    let now = timestamp();
    let receipts = receipt_map(
        &hosts,
        vec![
            ("m3".to_owned(), observation(&desired, true, false, true)),
            ("m1".to_owned(), observation(&desired, false, false, false)),
            ("m5".to_owned(), observation(&desired, true, false, false)),
        ],
        &desired,
        &now,
    );

    assert_eq!(next_wave(&hosts, &receipts, "m3", true), vec!["m5"]);
    assert_eq!(receipts["m1"].state, HostState::Offline);
}

#[test]
fn busy_host_does_not_keep_other_idle_hosts_out_of_the_post_canary_wave() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![
        host("m3", true, true),
        host("m1", false, false),
        host("m5", false, false),
    ];
    let receipts = receipt_map(
        &hosts,
        vec![
            ("m3".to_owned(), observation(&desired, true, false, true)),
            ("m1".to_owned(), observation(&desired, true, true, false)),
            ("m5".to_owned(), observation(&desired, true, false, false)),
        ],
        &desired,
        &timestamp(),
    );

    assert_eq!(next_wave(&hosts, &receipts, "m3", true), vec!["m5"]);
    assert_eq!(receipts["m1"].state, HostState::Busy);
}

#[test]
fn busy_host_with_exact_identity_is_still_converged() {
    let desired = identity("0.97.0", 'a');
    let observed = observation(&desired, true, true, true);
    assert_eq!(observed.disposition(&desired), HostState::Converged);
}

#[test]
fn an_unproven_prior_canary_is_reselected_after_it_goes_offline() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m3", true, true), host("m5", false, false)];
    let receipts = receipt_map(
        &hosts,
        vec![
            ("m3".to_owned(), observation(&desired, false, false, false)),
            ("m5".to_owned(), observation(&desired, true, false, false)),
        ],
        &desired,
        &timestamp(),
    );

    assert_eq!(select_canary(&hosts, &receipts, Some("m3"), false), "m5");
}

#[derive(Default)]
struct FixtureExecutor {
    probes: Mutex<BTreeMap<String, VecDeque<HostObservation>>>,
    installs: Mutex<Vec<String>>,
    install_errors: Mutex<VecDeque<String>>,
}

impl FixtureExecutor {
    fn seed(&self, name: &str, observations: Vec<HostObservation>) {
        self.probes
            .lock()
            .expect("lock")
            .insert(name.to_owned(), observations.into());
    }

    fn fail_next_install(&self, message: &str) {
        self.install_errors
            .lock()
            .expect("lock")
            .push_back(message.to_owned());
    }
}

impl HostExecutor for FixtureExecutor {
    fn probe(&self, host: &FleetHost) -> HostObservation {
        let mut probes = self.probes.lock().expect("lock");
        let queue = probes.get_mut(&host.name).expect("host fixture");
        if queue.len() > 1 {
            queue.pop_front().expect("probe")
        } else {
            queue.front().expect("last probe").clone()
        }
    }

    fn install(
        &self,
        host: &FleetHost,
        _desired: &ReleaseIdentity,
        _before: &HostObservation,
    ) -> Result<(), String> {
        self.installs.lock().expect("lock").push(host.name.clone());
        match self.install_errors.lock().expect("lock").pop_front() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[test]
fn rejoin_converges_offline_host_without_reinstalling_completed_peers() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![
        host("m3", true, true),
        host("m1", false, false),
        host("m5", false, false),
    ];
    let executor = FixtureExecutor::default();
    executor.seed("m3", vec![observation(&desired, true, false, true)]);
    executor.seed(
        "m1",
        vec![
            observation(&desired, false, false, false),
            observation(&desired, true, false, false),
            observation(&desired, true, false, true),
        ],
    );
    executor.seed("m5", vec![observation(&desired, true, false, true)]);
    let temp = tempfile::tempdir().expect("tempdir");
    let state_file = temp.path().join("state.json");
    let mut state = RolloutState {
        schema_version: STATE_SCHEMA,
        generation: "g1".to_owned(),
        direction: RolloutDirection::Forward,
        desired: desired.clone(),
        rollback: Some(identity("0.96.0", 'b')),
        hosts_file: temp.path().join("hosts.json"),
        inventory_sha256: "a".repeat(64),
        canary_host: Some("m3".to_owned()),
        canary_proven: true,
        hosts: BTreeMap::new(),
        reconciler: ReconcilerReceipt {
            installed: false,
            loaded: false,
            detail: "test".to_owned(),
        },
        updated_at: timestamp(),
    };

    reconcile_once(&executor, &hosts, &state_file, &mut state).expect("offline pass");
    assert_eq!(state.hosts["m1"].state, HostState::Offline);
    assert!(executor.installs.lock().expect("lock").is_empty());

    reconcile_once(&executor, &hosts, &state_file, &mut state).expect("rejoin pass");
    assert_eq!(state.hosts["m1"].state, HostState::Converged);
    assert_eq!(*executor.installs.lock().expect("lock"), vec!["m1"]);
}

#[test]
fn terminal_host_failure_survives_later_matching_probes() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m3", true, true), host("m5", false, false)];
    let executor = FixtureExecutor::default();
    executor.seed("m3", vec![observation(&desired, true, false, true)]);
    executor.seed("m5", vec![observation(&desired, true, false, true)]);
    let temp = tempfile::tempdir().expect("tempdir");
    let state_file = temp.path().join("state.json");
    let failed_observation = observation(&desired, true, false, true);
    let mut state = RolloutState {
        schema_version: STATE_SCHEMA,
        generation: "g1".to_owned(),
        direction: RolloutDirection::Forward,
        desired: desired.clone(),
        rollback: Some(identity("0.96.0", 'b')),
        hosts_file: temp.path().join("hosts.json"),
        inventory_sha256: "a".repeat(64),
        canary_host: Some("m3".to_owned()),
        canary_proven: true,
        hosts: BTreeMap::from([(
            "m5".to_owned(),
            HostReceipt {
                state: HostState::Failed,
                observed: failed_observation,
                expected_participation: Some(true),
                require_daemon_running: true,
                updated_at: timestamp(),
            },
        )]),
        reconciler: ReconcilerReceipt {
            installed: false,
            loaded: false,
            detail: "test".to_owned(),
        },
        updated_at: timestamp(),
    };

    reconcile_once(&executor, &hosts, &state_file, &mut state).expect("reconcile");
    assert_eq!(state.hosts["m5"].state, HostState::Failed);
    assert!(executor.installs.lock().expect("lock").is_empty());
}

#[test]
fn participation_change_is_a_terminal_host_failure_not_a_fleet_wide_stop() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m3", true, true), host("m5", false, false)];
    let executor = FixtureExecutor::default();
    executor.seed("m3", vec![observation(&desired, true, false, true)]);
    let mut m5_after = observation(&desired, true, false, true);
    m5_after.participation = Some(false);
    executor.seed(
        "m5",
        vec![observation(&desired, true, false, false), m5_after],
    );
    let now = timestamp();
    let initial = probe_hosts(&executor, &hosts);
    let mut receipts = receipt_map(&hosts, initial, &desired, &now);
    apply_wave(
        &executor,
        &hosts,
        &mut receipts,
        &desired,
        &["m5".to_owned()],
    );

    assert_eq!(receipts["m5"].state, HostState::Failed);
    assert_eq!(receipts["m3"].state, HostState::Converged);
    assert!(
        receipts["m5"]
            .observed
            .detail
            .contains("participation changed")
    );
}

#[test]
fn running_daemon_must_still_be_running_after_update() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m3", true, true)];
    let executor = FixtureExecutor::default();
    let before = observation(&desired, true, false, false);
    let mut after = observation(&desired, true, false, true);
    after.daemon_running = Some(false);
    after.daemon_version = None;
    executor.seed("m3", vec![after]);
    let mut receipts = receipt_map(
        &hosts,
        vec![("m3".to_owned(), before)],
        &desired,
        &timestamp(),
    );

    apply_wave(
        &executor,
        &hosts,
        &mut receipts,
        &desired,
        &["m3".to_owned()],
    );

    assert_eq!(receipts["m3"].state, HostState::Failed);
    assert!(receipts["m3"].observed.detail.contains("daemon"));
}

#[test]
fn host_that_becomes_busy_at_mutation_boundary_is_not_installed() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m3", true, true)];
    let executor = FixtureExecutor::default();
    executor.seed("m3", vec![observation(&desired, true, true, false)]);
    let mut receipts = receipt_map(
        &hosts,
        vec![("m3".to_owned(), observation(&desired, true, false, false))],
        &desired,
        &timestamp(),
    );

    apply_wave(
        &executor,
        &hosts,
        &mut receipts,
        &desired,
        &["m3".to_owned()],
    );

    assert_eq!(receipts["m3"].state, HostState::Busy);
    assert!(executor.installs.lock().expect("lock").is_empty());
}

#[test]
fn transient_install_disconnect_remains_retryable_and_converges_on_rejoin() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m5", false, true)];
    let executor = FixtureExecutor::default();
    executor.fail_next_install("ssh disconnected");
    executor.seed(
        "m5",
        vec![
            observation(&desired, false, false, false),
            observation(&desired, true, false, true),
        ],
    );
    let before = observation(&desired, true, false, false);
    let mut receipts = receipt_map(
        &hosts,
        vec![("m5".to_owned(), before)],
        &desired,
        &timestamp(),
    );

    apply_wave(
        &executor,
        &hosts,
        &mut receipts,
        &desired,
        &["m5".to_owned()],
    );
    assert_eq!(receipts["m5"].state, HostState::Offline);

    receipts.insert(
        "m5".to_owned(),
        HostReceipt {
            state: HostState::Pending,
            observed: observation(&desired, true, false, false),
            expected_participation: Some(true),
            require_daemon_running: true,
            updated_at: timestamp(),
        },
    );
    apply_wave(
        &executor,
        &hosts,
        &mut receipts,
        &desired,
        &["m5".to_owned()],
    );
    assert_eq!(receipts["m5"].state, HostState::Converged);
    assert_eq!(receipts["m5"].expected_participation, None);
    assert!(!receipts["m5"].require_daemon_running);
}

#[test]
fn inconclusive_probe_keeps_preupdate_invariants_for_later_rejoin() {
    let desired = identity("0.97.0", 'a');
    let hosts = vec![host("m5", false, true)];
    let executor = FixtureExecutor::default();
    let mut rejoined = observation(&desired, true, false, true);
    rejoined.daemon_running = Some(false);
    rejoined.daemon_version = None;
    rejoined.participation = Some(false);
    executor.seed("m5", vec![rejoined]);
    let temp = tempfile::tempdir().expect("tempdir");
    let state_file = temp.path().join("state.json");
    let mut state = RolloutState {
        schema_version: STATE_SCHEMA,
        generation: "g1".to_owned(),
        direction: RolloutDirection::Forward,
        desired: desired.clone(),
        rollback: Some(identity("0.96.0", 'b')),
        hosts_file: temp.path().join("hosts.json"),
        inventory_sha256: "a".repeat(64),
        canary_host: Some("m5".to_owned()),
        canary_proven: false,
        hosts: BTreeMap::from([(
            "m5".to_owned(),
            HostReceipt {
                state: HostState::Offline,
                observed: observation(&desired, false, false, false),
                expected_participation: Some(true),
                require_daemon_running: true,
                updated_at: timestamp(),
            },
        )]),
        reconciler: ReconcilerReceipt {
            installed: false,
            loaded: false,
            detail: "test".to_owned(),
        },
        updated_at: timestamp(),
    };

    reconcile_once(&executor, &hosts, &state_file, &mut state).expect("rejoin probe");

    assert_eq!(state.hosts["m5"].state, HostState::Failed);
    assert_eq!(state.hosts["m5"].expected_participation, Some(true));
    assert!(state.hosts["m5"].require_daemon_running);
    assert!(state.hosts["m5"].observed.detail.contains("participation"));
}

#[test]
fn installer_rejects_wrong_expected_digest_before_replacing_binary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let binary = temp.path().join("shipyard");
    fs::write(&binary, b"original bytes").expect("fixture binary");
    let original = fs::read(&binary).expect("original bytes");
    let output = std::process::Command::new("bash")
        .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh"))
        .env("SHIPYARD_INSTALL_DIR", temp.path())
        .env("SHIPYARD_SKIP_DOWNLOAD", "1")
        .env("SHIPYARD_SKIP_SMOKE", "1")
        .env("SHIPYARD_VERSION", "v0.97.0")
        .env("SHIPYARD_EXPECTED_BINARY_SHA256", "f".repeat(64))
        .output()
        .expect("run installer");

    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("SHA-256 mismatch"));
    assert_eq!(fs::read(&binary).expect("retained binary"), original);
}

#[test]
fn rollback_is_exact_and_swaps_forward_identity_for_recovery() {
    let forward = identity("0.97.0", 'a');
    let backward = identity("0.96.0", 'b');
    let mut state = RolloutState {
        schema_version: STATE_SCHEMA,
        generation: "forward".to_owned(),
        direction: RolloutDirection::Forward,
        desired: forward.clone(),
        rollback: Some(backward.clone()),
        hosts_file: PathBuf::from("hosts.json"),
        inventory_sha256: "a".repeat(64),
        canary_host: Some("m3".to_owned()),
        canary_proven: true,
        hosts: BTreeMap::new(),
        reconciler: ReconcilerReceipt {
            installed: true,
            loaded: true,
            detail: "test".to_owned(),
        },
        updated_at: timestamp(),
    };

    activate_rollback(&mut state).expect("rollback");
    assert_eq!(state.desired, backward);
    assert_eq!(state.rollback, Some(forward));
    assert_eq!(state.direction, RolloutDirection::Rollback);

    let once = state.clone();
    activate_rollback(&mut state).expect("idempotent rollback");
    assert_eq!(state.desired, once.desired);
    assert_eq!(state.rollback, once.rollback);
}

#[test]
fn rollback_preserves_owned_drain_recovery_expectations() {
    let forward = identity("0.97.0", 'a');
    let backward = identity("0.96.0", 'b');
    let mut drained = observation(&forward, true, false, false);
    drained.participation = Some(false);
    drained.drain_owned = Some(true);
    let mut state = RolloutState {
        schema_version: STATE_SCHEMA,
        generation: "forward".to_owned(),
        direction: RolloutDirection::Forward,
        desired: forward,
        rollback: Some(backward.clone()),
        hosts_file: PathBuf::from("hosts.json"),
        inventory_sha256: "a".repeat(64),
        canary_host: Some("m3".to_owned()),
        canary_proven: false,
        hosts: BTreeMap::from([(
            "m3".to_owned(),
            HostReceipt {
                state: HostState::Offline,
                observed: drained,
                expected_participation: Some(true),
                require_daemon_running: true,
                updated_at: timestamp(),
            },
        )]),
        reconciler: ReconcilerReceipt {
            installed: true,
            loaded: true,
            detail: "test".to_owned(),
        },
        updated_at: timestamp(),
    };

    activate_rollback(&mut state).expect("rollback");

    assert_eq!(state.desired, backward);
    assert_eq!(state.hosts["m3"].expected_participation, Some(true));
    assert!(state.hosts["m3"].require_daemon_running);
    assert_eq!(state.hosts["m3"].observed.drain_owned, Some(true));
}

#[test]
fn legacy_inventory_adds_local_controller_and_rejects_duplicate_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("fleet-hosts.json");
    fs::write(
        &path,
        r#"[{"name":"M1","ssh":"m1"},{"name":"M5","ssh":"m5"}]"#,
    )
    .expect("write");
    let hosts = load_hosts(&path).expect("legacy inventory");
    assert_eq!(hosts.iter().filter(|host| host.local).count(), 1);
    assert_eq!(hosts.len(), 3);

    fs::write(
        &path,
        r#"[{"name":"M1","ssh":"m1"},{"name":"m1","ssh":"other"}]"#,
    )
    .expect("write duplicate");
    assert!(load_hosts(&path).is_err());

    fs::write(
        &path,
        r#"[{"name":"M1","ssh":"-oProxyCommand=touch /tmp/nope"}]"#,
    )
    .expect("write unsafe ssh alias");
    assert!(load_hosts(&path).is_err());

    fs::write(
        &path,
        r#"{"schema_version":1,"hosts":[{"name":"M1","ssh":"m1"}]}"#,
    )
    .expect("write explicit remote-only inventory");
    assert!(load_hosts(&path).is_err());
}

#[test]
fn probe_parser_requires_daemon_and_participation_observability() {
    let desired = identity("0.97.0", 'a');
    let parsed = parse_probe(&format!(
        "version=0.97.0\nsha256={}\ndaemon_running=true\ndaemon_version=0.97.0\nparticipation=true\ndrain_owned=false\nbusy=false\n",
        desired.sha256
    ));
    assert_eq!(parsed.disposition(&desired), HostState::Converged);

    let incomplete = parse_probe(&format!(
        "version=0.97.0\nsha256={}\ndaemon_running=\ndaemon_version=\nparticipation=unknown\ndrain_owned=false\nbusy=false\n",
        desired.sha256
    ));
    assert_eq!(incomplete.disposition(&desired), HostState::Unobservable);
}

#[test]
fn probe_supports_macos_and_linux_digest_tools() {
    assert!(PROBE_SCRIPT.contains("command -v shasum"));
    assert!(PROBE_SCRIPT.contains("command -v sha256sum"));
    assert!(PROBE_SCRIPT.contains("openssl dgst -sha256"));
}

#[test]
fn missing_binary_is_pending_and_bootstrappable_when_host_is_otherwise_observable() {
    let desired = identity("0.97.0", 'a');
    let parsed = parse_probe(
        "version=missing\nsha256=missing\ndaemon_running=false\ndaemon_version=\nparticipation=true\ndrain_owned=false\nbusy=false\n",
    );
    assert_eq!(parsed.disposition(&desired), HostState::Pending);
}

#[test]
fn plist_pins_state_and_inventory_for_eventual_offline_convergence() {
    let plist = reconciler_plist(
        Path::new("/Users/d/.local/bin/shipyard"),
        Path::new("/Users/d/state.json"),
        Path::new("/Users/d/fleet-hosts.json"),
        Path::new("/Users/d/logs"),
    );
    assert!(plist.contains("<string>reconcile</string>"));
    assert!(plist.contains("/Users/d/state.json"));
    assert!(plist.contains("StartInterval"));
    assert!(plist.contains("300"));
}

#[test]
fn state_lock_refuses_a_competing_reconciler() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state.json");
    let _owner = StateLock::acquire(&state).expect("first owner");
    assert!(StateLock::acquire(&state).is_err());
}

#[test]
fn timeout_or_lost_ssh_never_authorizes_immediate_participation_restore() {
    let desired = identity("0.97.0", 'a');
    let mut still_mutating = observation(&desired, true, true, false);
    still_mutating.participation = Some(false);
    still_mutating.drain_owned = Some(true);
    assert!(!owns_recoverable_drain(Some(true), &still_mutating));

    still_mutating.busy = Some(false);
    assert!(owns_recoverable_drain(Some(true), &still_mutating));

    still_mutating.drain_owned = Some(false);
    assert!(!owns_recoverable_drain(Some(true), &still_mutating));
}
