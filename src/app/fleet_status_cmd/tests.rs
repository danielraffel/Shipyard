use super::*;
use crate::fleet_slot::QueuedJobState;

#[cfg(unix)]
fn executable_script(path: &Path, body: &str) {
    crate::test_support::write_executable_script_with_mode(
        path,
        &format!("#!/bin/sh\nset -eu\n{body}\n"),
        0o700,
    );
}

#[cfg(unix)]
fn isolated_storage_probe_until(class: &HostClassConfig, deadline: Instant) -> StorageProbe {
    let disk_path = class.tart_home.as_deref().unwrap_or(".");
    let script = storage_probe_script(disk_path);
    let mut command = Command::new("sh");
    command
        .args(["-c", &script])
        // Keep this concurrency control independent of the host's live ccache.
        // Dedicated tests below cover ccache discovery and parsing.
        .env("PATH", "/usr/bin:/bin");
    match run_output_until(&mut command, deadline, "isolated storage probe") {
        Ok(output) => storage_probe_from_output(&output, disk_path),
        Err(error) => StorageProbe {
            source: error.to_string(),
            disk_path: disk_path.to_owned(),
            disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
            ..StorageProbe::default()
        },
    }
}

fn wedge_runner(name: &str, status: &str, busy: bool, labels: &[&str]) -> RepositoryRunner {
    RepositoryRunner {
        id: 900,
        name: name.to_owned(),
        status: status.to_owned(),
        busy,
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
    }
}

/// A repo-scope census that answered and found the lane served.
fn corroborated_lane() -> LaneCorroboration {
    LaneCorroboration {
        inventory_readable: true,
        online_lane_runners: 2,
    }
}

/// A repo-scope census that could not be read at all.
fn uncorroborated_lane() -> LaneCorroboration {
    LaneCorroboration {
        inventory_readable: false,
        online_lane_runners: 0,
    }
}

fn wedge_inventory(runners: Vec<RepositoryRunner>) -> RunnerInventory {
    RunnerInventory {
        boundary: None,
        attempts: 1,
        readable: true,
        source: "github".to_owned(),
        runners,
    }
}

fn wedge_run(created_at: &str, job_labels: &[&str]) -> ActiveRunObservation {
    ActiveRunObservation {
        run_id: 4242,
        workflow: "Build and Test".to_owned(),
        head_branch: "main".to_owned(),
        head_sha: Some("cafebabe".to_owned()),
        status: "queued".to_owned(),
        created_at: Some(created_at.to_owned()),
        pull_requests: vec![],
        url: None,
        jobs: vec![JobObservation {
            name: "macOS (arm64)".to_owned(),
            status: "queued".to_owned(),
            runner_name: None,
            labels: job_labels.iter().map(|label| (*label).to_owned()).collect(),
        }],
    }
}

fn wedge_now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-09-12T12:00:00Z")
        .expect("fixed clock parses")
        .with_timezone(&Utc)
}

#[test]
fn wedged_queued_job_raises_while_a_capable_runner_sits_idle() {
    // The incident shape: a run queued far past the threshold whose labels two
    // online, idle runners advertise. Every liveness-shaped reading is green —
    // the runners are registered, online, and answering — and the work is not
    // moving.
    let inventory = wedge_inventory(vec![
        wedge_runner(
            "pulp-build-m5",
            "online",
            false,
            &["self-hosted", "macOS", "ARM64", "pulp-build-pr-head"],
        ),
        wedge_runner("pulp-build-m1", "online", false, &["self-hosted", "macOS"]),
    ]);
    let runs = vec![wedge_run(
        "2026-09-12T09:00:00Z",
        &["self-hosted", "macos", "arm64"],
    )];

    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());

    assert_eq!(found.examined, 1, "the sweep must report what it looked at");
    assert_eq!(found.raising.len(), 1);
    assert_eq!(found.raising[0].state, QueuedJobState::Wedged);
    assert!(found.raising[0].verdict.is_raise());
    assert_eq!(found.raising[0].queued_secs, 10_800);
}

#[test]
fn control_a_busy_capable_runner_is_saturation_not_a_wedge() {
    // The planted negative control for the assertion above. The only change is
    // that the capable runner is busy, which is ordinary saturation. If this
    // ever raises, the check is reporting a full fleet as a broken one.
    let inventory = wedge_inventory(vec![wedge_runner(
        "pulp-build-m5",
        "online",
        true,
        &["self-hosted", "macOS", "ARM64"],
    )]);
    let runs = vec![wedge_run(
        "2026-09-12T09:00:00Z",
        &["self-hosted", "macos", "arm64"],
    )];

    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());

    assert_eq!(
        found.examined, 1,
        "a silent instrument and a clean pass must not read alike"
    );
    assert!(found.raising.is_empty());
}

#[test]
fn control_an_unlabelled_job_cannot_manufacture_a_wedge() {
    // `advertises_all` is a superset test and is vacuously true on an empty
    // slice, so without the guard an unlabelled job matches every runner and
    // is reported as wedged on the strength of a runner that could never have
    // been assigned it.
    let inventory = wedge_inventory(vec![wedge_runner(
        "pulp-build-m5",
        "online",
        false,
        &["self-hosted", "macOS", "ARM64"],
    )]);
    let runs = vec![wedge_run("2026-09-12T09:00:00Z", &[])];

    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());

    assert_eq!(found.examined, 0);
    assert!(found.raising.is_empty());
}

#[test]
fn control_a_started_run_does_not_lend_its_runtime_to_a_downstream_job() {
    // A job can enter the queue long after its workflow starts, and the run's
    // `created_at` is the only age proxy available. Reading it on a run that is
    // already in progress turns upstream runtime into queue age.
    let mut runs = vec![wedge_run(
        "2026-09-12T09:00:00Z",
        &["self-hosted", "macos", "arm64"],
    )];
    runs[0].status = "in_progress".to_owned();
    let inventory = wedge_inventory(vec![wedge_runner(
        "pulp-build-m5",
        "online",
        false,
        &["self-hosted", "macOS", "ARM64"],
    )]);

    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());

    assert_eq!(found.examined, 0);
    assert!(found.raising.is_empty());
}

#[test]
fn control_an_unreadable_census_reports_nothing_rather_than_a_pass() {
    // A census that could not be read cannot establish a wedge, and must not be
    // folded into a clean result. `examined = 0` is the signal that the sweep
    // reached nothing.
    let inventory = RunnerInventory {
        boundary: None,
        attempts: 1,
        readable: false,
        source: "github: rate limited".to_owned(),
        runners: vec![],
    };
    let runs = vec![wedge_run(
        "2026-09-12T09:00:00Z",
        &["self-hosted", "macos", "arm64"],
    )];

    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());

    assert_eq!(found.examined, 0);
    assert!(found.raising.is_empty());
}

#[test]
fn no_capable_runner_defers_to_the_lane_service_assertion() {
    // Nothing online advertises the labels. That is a routing question, and the
    // repository-scoped census here cannot see an org-scoped runner — so the
    // verdict must stay non-raising rather than claim a wedge it cannot prove.
    let inventory = wedge_inventory(vec![wedge_runner(
        "pulp-build-linux",
        "online",
        false,
        &["self-hosted", "Linux", "X64"],
    )]);
    let runs = vec![wedge_run(
        "2026-09-12T09:00:00Z",
        &["self-hosted", "macos", "arm64"],
    )];

    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());

    assert_eq!(found.examined, 1);
    assert!(found.raising.is_empty());
}

fn wedge_assessment(wedged_queued: WedgedQueuedJobs) -> FleetAssessment {
    FleetAssessment {
        repo: "owner/repo".to_owned(),
        target: "macos".to_owned(),
        free: 2,
        routable_free_slots: 1,
        routing_confidence: RoutingConfidence::Confirmed,
        routing_degraded_reasons: Vec::new(),
        capacity_unreadable: false,
        doctor_unreadable: false,
        supervisor_unhealthy: false,
        problem_hosts: false,
        queued_age_threshold_secs: 900,
        queue_run_limit: 50,
        queued_age_with_capacity: false,
        queue: QueuedSummary {
            readable: true,
            source: "github".to_owned(),
            count: 0,
            oldest_age_secs: None,
        },
        base: "main".to_owned(),
        merge_queue_stall_threshold_secs: 900,
        merge_queue: MergeQueueProbe {
            readable: true,
            source: "github".to_owned(),
            report: None,
            reason_codes: Vec::new(),
        },
        release_stale_threshold_secs: 86_400,
        release: ReleaseProbe {
            readable: true,
            source: "github".to_owned(),
            report: None,
            reason_codes: Vec::new(),
        },
        hosts: Vec::new(),
        runners: RunnerInventory {
            boundary: None,
            attempts: 1,
            readable: true,
            source: "github".to_owned(),
            runners: Vec::new(),
        },
        expected_hosts: Vec::new(),
        routing_mismatches: Vec::new(),
        wedged_queued,
        observation_reason_codes: Vec::new(),
        observation_incomplete: false,
        should_fail: false,
    }
}

#[test]
fn a_wedged_queued_finding_reaches_both_json_surfaces() {
    // A detector whose output never renders is another orphan. The one-shot
    // command envelope and the watch event share one writer, so the finding has
    // to arrive on both or the assertion is unobservable from either.
    let inventory = wedge_inventory(vec![wedge_runner(
        "pulp-build-m5",
        "online",
        false,
        &["self-hosted", "macOS", "ARM64"],
    )]);
    let runs = vec![wedge_run(
        "2026-09-12T09:00:00Z",
        &["self-hosted", "macos", "arm64"],
    )];
    let found =
        detect_wedged_queued_jobs(&runs, &inventory, WedgeThresholds::default(), wedge_now());
    assert_eq!(found.raising.len(), 1);

    let assessment = wedge_assessment(found);
    let mut command_output = Vec::new();
    render_fleet_assessment(&assessment, true, &mut command_output).expect("command JSON");
    let command: Value = serde_json::from_slice(&command_output).expect("command document");
    let mut watch_output = Vec::new();
    render_fleet_watch_event(&assessment, &mut watch_output).expect("watch JSON");
    let watch: Value = serde_json::from_slice(&watch_output).expect("watch document");

    assert_eq!(command["wedged_queued_jobs"]["examined"], 1);
    assert_eq!(
        command["wedged_queued_jobs"]["raising"][0]["state"],
        "wedged"
    );
    assert_eq!(
        command["wedged_queued_jobs"]["raising"][0]["queued_secs"],
        10_800
    );
    assert_eq!(command["wedged_queued_jobs"], watch["wedged_queued_jobs"]);
}

#[test]
fn control_a_sweep_that_found_nothing_still_reports_what_it_examined() {
    // "Looked at three and found no wedge" and "looked at nothing" must not
    // render identically — that difference is the only thing separating a clean
    // pass from a silent instrument.
    let looked = wedge_assessment(WedgedQueuedJobs {
        examined: 3,
        raising: Vec::new(),
    });
    let mut looked_output = Vec::new();
    render_fleet_assessment(&looked, true, &mut looked_output).expect("command JSON");
    let looked_document: Value = serde_json::from_slice(&looked_output).expect("command document");

    assert_eq!(looked_document["wedged_queued_jobs"]["examined"], 3);
    assert_eq!(
        looked_document["wedged_queued_jobs"]["raising"],
        serde_json::json!([])
    );

    let blind = wedge_assessment(WedgedQueuedJobs::default());
    let mut blind_output = Vec::new();
    render_fleet_assessment(&blind, true, &mut blind_output).expect("command JSON");
    let blind_document: Value = serde_json::from_slice(&blind_output).expect("command document");

    assert_eq!(blind_document["wedged_queued_jobs"]["examined"], 0);
    assert_ne!(
        looked_document["wedged_queued_jobs"],
        blind_document["wedged_queued_jobs"]
    );
}

#[cfg(unix)]
#[test]
fn fleet_github_reads_share_one_absolute_deadline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let hanging_gh = temp.path().join("hanging-gh");
    executable_script(&hanging_gh, "sleep 30");
    let config = LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    let actions = GitHubActions::from_loaded_config(temp.path(), &config)
        .with_gh_binary_for_tests(&hanging_gh);
    let bounded = fleet_github_actions_with_timeout(&actions, Duration::from_millis(150));
    let started = Instant::now();

    let error = bounded
        .run_gh(&["api".to_owned(), "repos/example/project".to_owned()])
        .expect_err("hung GitHub observation must time out");

    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(error.to_string().contains("timed out"));
    let retry_started = Instant::now();
    let expired = bounded
        .run_gh(&["api".to_owned(), "repos/example/project".to_owned()])
        .expect_err("later reads must not receive a fresh timeout");
    assert!(retry_started.elapsed() < Duration::from_secs(1));
    assert!(
        expired.to_string().contains("absolute deadline")
            || expired.to_string().contains("timed out")
    );
}

#[cfg(unix)]
#[test]
#[allow(clippy::too_many_lines)] // One end-to-end mixed-host observer fixture.
fn mixed_healthy_and_timed_out_hosts_finish_under_one_deadline() {
    let temp = tempfile::tempdir().expect("tempdir");
    let hanging_tart = temp.path().join("hanging-tart");
    let healthy_tart = temp.path().join("healthy-tart");
    let healthy_tartci = temp.path().join("healthy-tartci");
    executable_script(&hanging_tart, "sleep 30");
    executable_script(&healthy_tart, "printf '[]'");
    executable_script(
        &healthy_tartci,
        r#"printf '%s' '{"config":{"heartbeat_stale_secs":900},"problems":[],"supervisors":[{"labels":["self-hosted","macOS","ARM64"],"owner_pid_alive":true,"heartbeat_age_secs":1}]}'"#,
    );
    let classes = vec![
        HostClassConfig {
            class: "blocked".to_owned(),
            ssh: None,
            cap: 2,
            tart_bin: hanging_tart.display().to_string(),
            tartci_bin: healthy_tartci.display().to_string(),
            shipyard_bin: None,
            shipyard_mode: None,
            shipyard_global_dir: None,
            shipyard_state_dir: None,
            github_cli: None,
            github_token_helper: None,
            tart_home: None,
            labels: Vec::new(),
        },
        HostClassConfig {
            class: "healthy".to_owned(),
            ssh: None,
            cap: 2,
            tart_bin: healthy_tart.display().to_string(),
            tartci_bin: healthy_tartci.display().to_string(),
            shipyard_bin: None,
            shipyard_mode: None,
            shipyard_global_dir: None,
            shipyard_state_dir: None,
            github_cli: None,
            github_token_helper: None,
            tart_home: None,
            labels: Vec::new(),
        },
    ];

    // Exercise the production host-observation contract. A shorter test-only
    // deadline makes a healthy shell probe race the fully parallel test suite
    // even though production would still accept it.
    let timeout = FLEET_HOST_PROBE_TIMEOUT;
    let started = std::time::Instant::now();
    let mut probes = probe_hosts_concurrently_with_timeout_using(
        &classes,
        timeout,
        isolated_storage_probe_until,
    );

    assert!(started.elapsed() < timeout + std::time::Duration::from_secs(5));
    assert_eq!(probes.len(), 2);
    assert!(!probes[0].capacity.readable());
    assert_eq!(probes[0].capacity.free(), 0);
    assert!(probes[0].capacity.source.contains("timed out"));
    assert!(
        probes[1].capacity.readable(),
        "healthy capacity source: {}",
        probes[1].capacity.source
    );
    assert_eq!(probes[1].capacity.free(), 2);
    assert!(
        probes[1].doctor.readable,
        "healthy doctor source: {}",
        probes[1].doctor.source
    );
    assert!(
        probes[1].storage.readable || probes[1].storage.source.contains("timed out"),
        "healthy storage source: {}",
        probes[1].storage.source
    );

    // Host observation deliberately includes the canonical ambient ccache. In
    // a saturated parallel suite its stats read may consume the shared
    // deadline; that fail-closed result is valid and is not evidence that the
    // otherwise healthy host failed to run concurrently. Storage parsing and
    // thresholds have deterministic tests below, so normalize the observation
    // only for the pure routability assertion.
    probes[1].storage.readable = true;
    probes[1].storage.source = "test fixture".to_owned();
    probes[1].storage.disk_available_kibibyte = Some(DEFAULT_DISK_FLOOR_KIBIBYTE.saturating_mul(2));
    probes[1].storage.ccache_size_kibibyte = Some(1);
    probes[1].storage.ccache_max_kibibyte = Some(2);
    // The healthy class declares no ssh host, so its attestation probe reads
    // THIS machine's real artifact and reports whatever the live fleet is doing.
    // Replace the whole observation rather than patching `readable`: a live
    // crash-looping runner would otherwise decide a concurrency test's verdict.
    probes[1].attestation = AttestationProbe {
        readable: true,
        source: "test fixture".to_owned(),
        ..AttestationProbe::default()
    };

    let hosts = probes
        .into_iter()
        .map(|probe| {
            analyze_host(
                probe.capacity,
                probe.doctor,
                probe.storage,
                probe.attestation,
                FLEET_LANE_TARGET,
                corroborated_lane(),
            )
        })
        .collect::<Vec<_>>();
    assert!(!hosts[0].routable);
    assert!(hosts[1].routable);
    let assessment = FleetAssessment {
        repo: "owner/repo".to_owned(),
        target: "macos".to_owned(),
        free: 2,
        routable_free_slots: 2,
        routing_confidence: RoutingConfidence::Confirmed,
        routing_degraded_reasons: Vec::new(),
        capacity_unreadable: true,
        doctor_unreadable: true,
        supervisor_unhealthy: false,
        problem_hosts: true,
        queued_age_threshold_secs: 900,
        queue_run_limit: 50,
        queued_age_with_capacity: false,
        queue: QueuedSummary {
            readable: true,
            source: "github".to_owned(),
            count: 0,
            oldest_age_secs: None,
        },
        base: "main".to_owned(),
        merge_queue_stall_threshold_secs: 900,
        merge_queue: MergeQueueProbe {
            readable: true,
            source: "github".to_owned(),
            report: None,
            reason_codes: Vec::new(),
        },
        release_stale_threshold_secs: 86_400,
        release: ReleaseProbe {
            readable: true,
            source: "github".to_owned(),
            report: None,
            reason_codes: Vec::new(),
        },
        hosts,
        runners: RunnerInventory {
            boundary: None,
            attempts: 1,
            readable: true,
            source: "github".to_owned(),
            runners: Vec::new(),
        },
        expected_hosts: Vec::new(),
        routing_mismatches: Vec::new(),
        wedged_queued: WedgedQueuedJobs::default(),
        observation_reason_codes: Vec::new(),
        observation_incomplete: false,
        should_fail: true,
    };
    let mut rendered = Vec::new();
    render_fleet_assessment(&assessment, true, &mut rendered).expect("mixed fleet JSON");
    let document: Value = serde_json::from_slice(&rendered).expect("valid mixed fleet JSON");
    assert_eq!(document["hosts"][0]["routable"], false);
    assert_eq!(document["hosts"][1]["routable"], true);
    assert_eq!(document["routable_free_slots"], 2);
}

#[test]
fn assessment_renders_command_and_watch_json_without_round_trip() {
    let assessment = FleetAssessment {
        repo: "owner/repo".to_owned(),
        target: "macos".to_owned(),
        free: 2,
        routable_free_slots: 1,
        routing_confidence: RoutingConfidence::Confirmed,
        routing_degraded_reasons: Vec::new(),
        capacity_unreadable: false,
        doctor_unreadable: false,
        supervisor_unhealthy: false,
        problem_hosts: false,
        queued_age_threshold_secs: 900,
        queue_run_limit: 50,
        queued_age_with_capacity: false,
        queue: QueuedSummary {
            readable: true,
            source: "github".to_owned(),
            count: 0,
            oldest_age_secs: None,
        },
        base: "main".to_owned(),
        merge_queue_stall_threshold_secs: 900,
        merge_queue: MergeQueueProbe {
            readable: true,
            source: "github".to_owned(),
            report: None,
            reason_codes: Vec::new(),
        },
        release_stale_threshold_secs: 86_400,
        release: ReleaseProbe {
            readable: true,
            source: "github".to_owned(),
            report: None,
            reason_codes: Vec::new(),
        },
        hosts: Vec::new(),
        runners: RunnerInventory {
            boundary: None,
            attempts: 1,
            readable: true,
            source: "github".to_owned(),
            runners: Vec::new(),
        },
        expected_hosts: Vec::new(),
        routing_mismatches: Vec::new(),
        wedged_queued: WedgedQueuedJobs::default(),
        observation_reason_codes: Vec::new(),
        observation_incomplete: false,
        should_fail: false,
    };
    let mut command_output = Vec::new();
    render_fleet_assessment(&assessment, true, &mut command_output).expect("command JSON");
    let command: Value = serde_json::from_slice(&command_output).expect("command document");
    let mut watch_output = Vec::new();
    render_fleet_watch_event(&assessment, &mut watch_output).expect("watch JSON");
    let watch: Value = serde_json::from_slice(&watch_output).expect("watch document");

    assert_eq!(command["command"], "runner.fleet-status");
    assert!(command.get("event").is_none());
    assert_eq!(watch["command"], "runner.watch");
    assert_eq!(watch["event"], "fleet_liveness");
    assert_eq!(command["repo"], watch["repo"]);
    assert_eq!(command["merge_queue"], watch["merge_queue"]);
    assert_eq!(assessment.exit_code(), ExitCode::SUCCESS);
}

#[test]
fn supervisor_fresh_requires_alive_owner_and_recent_heartbeat() {
    let supervisor = serde_json::json!({
        "owner_pid_alive": true,
        "heartbeat_age_secs": 42
    });
    assert!(supervisor_is_fresh(&supervisor, 900));
    assert!(!supervisor_is_fresh(&supervisor, 10));
    let dead = serde_json::json!({
        "owner_pid_alive": false,
        "heartbeat_age_secs": 1
    });
    assert!(!supervisor_is_fresh(&dead, 900));
}

#[test]
fn remote_tartci_command_sets_tart_home_and_quotes_binary() {
    let class = HostClassConfig {
        class: "m5".to_owned(),
        ssh: Some("m5-ci".to_owned()),
        cap: 2,
        tart_bin: "/opt/homebrew/bin/tart".to_owned(),
        tartci_bin: "/Users/ci user/.local/bin/tartci".to_owned(),
        shipyard_bin: Some("/Users/ci user/.local/bin/shipyard".to_owned()),
        shipyard_mode: Some("shipyard".to_owned()),
        shipyard_global_dir: None,
        shipyard_state_dir: None,
        github_cli: Some("ghapp".to_owned()),
        github_token_helper: None,
        tart_home: Some("/Users/ci user/VMs".to_owned()),
        labels: Vec::new(),
    };
    assert_eq!(
        remote_tartci_command(&class),
        "env PATH=/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:/usr/bin:/bin:/usr/sbin:/sbin TART_HOME='/Users/ci user/VMs' TARTCI_GH_CLI=ghapp '/Users/ci user/.local/bin/tartci' doctor --reap --json"
    );
}

#[test]
fn remote_tartci_command_leaves_github_cli_unset_by_default() {
    let class = HostClassConfig {
        class: "studio".to_owned(),
        ssh: Some("studio".to_owned()),
        cap: 2,
        tart_bin: "tart".to_owned(),
        tartci_bin: "tartci".to_owned(),
        shipyard_bin: None,
        shipyard_mode: None,
        shipyard_global_dir: None,
        shipyard_state_dir: None,
        github_cli: None,
        github_token_helper: None,
        tart_home: None,
        labels: Vec::new(),
    };

    assert!(!remote_tartci_command(&class).contains("TARTCI_GH_CLI"));
}

#[test]
fn composite_platform_target_matches_lane_labels() {
    let labels = serde_json::json!(["self-hosted", "macOS", "ARM64"]);

    assert!(labels_match_target(&labels, "macos-arm64"));
    assert!(labels_match_target(&labels, "darwin-arm64"));
    assert!(!labels_match_target(&labels, "linux-arm64"));
}

#[test]
fn fleet_lane_is_independent_of_custom_queue_job_name() {
    let custom_queue_target = "required-apple-tests";
    let labels = serde_json::json!(["self-hosted", "macOS", "ARM64"]);

    assert!(!labels_match_target(&labels, custom_queue_target));
    assert!(labels_match_target(&labels, FLEET_LANE_TARGET));
}

#[test]
fn analyze_host_scopes_health_to_requested_target() {
    let doctor = DoctorProbe {
        readable: true,
        source: "test".to_owned(),
        digest: Some(serde_json::json!({
            "config": {"heartbeat_stale_secs": 900},
            "problems": ["suspect_live_owner_stale_heartbeat:linux-ephr-1"],
            "supervisors": [
                {"runner":"pulp-vm-01", "vm":"pulp-vm-01-x", "labels":"self-hosted,macOS,ARM64", "owner_pid_alive":true, "heartbeat_age_secs":5},
                {"runner":"linux-ephr-1", "vm":"linux-ephr-1", "labels":"self-hosted,Linux,ARM64", "owner_pid_alive":true, "heartbeat_age_secs":5000}
            ],
            "vms": [
                {"name":"linux-ephr-1", "stale":true}
            ],
            "github_runners": [
                {"name":"pulp-vm-01", "labels":["self-hosted", "macOS", "ARM64"]},
                {"name":"linux-ephr-1", "labels":["self-hosted", "Linux", "ARM64"]}
            ]
        })),
    };
    let host = analyze_host(
        HostCapacity {
            class: "studio".to_owned(),
            ssh: None,
            cap: 2,
            running: Some(0),
            source: "test".to_owned(),
        },
        doctor,
        StorageProbe {
            readable: true,
            source: "test".to_owned(),
            disk_path: "/tmp".to_owned(),
            disk_available_kibibyte: Some(DEFAULT_DISK_FLOOR_KIBIBYTE * 2),
            disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
            ccache_size_kibibyte: Some(1),
            ccache_max_kibibyte: Some(2),
        },
        healthy_attestation(),
        FLEET_LANE_TARGET,
        corroborated_lane(),
    );
    assert!(host.routable);
    assert_eq!(host.problem_count, 0);
    assert_eq!(host.supervisor_count, 1);
    assert_eq!(host.github_runner_count, 1);
    assert_eq!(host.stale_vm_count, 0);
}

#[test]
fn central_runner_inventory_supersedes_host_github_rate_limit_problem() {
    let doctor = DoctorProbe {
        readable: true,
        source: "test".to_owned(),
        digest: Some(serde_json::json!({
            "config": {"heartbeat_stale_secs": 900},
            "problems": ["github_unreadable:HTTP 403 rate limit exceeded"],
            "supervisors": [{
                "runner":"pulp-vm-m5-01",
                "labels":"self-hosted,macOS,ARM64",
                "owner_pid_alive":true,
                "heartbeat_age_secs":5
            }]
        })),
    };
    let capacity = HostCapacity {
        class: "m5".to_owned(),
        ssh: Some("m5".to_owned()),
        cap: 2,
        running: Some(0),
        source: "test".to_owned(),
    };
    let storage = StorageProbe {
        readable: true,
        source: "test".to_owned(),
        disk_path: "/Users/ci/VMs".to_owned(),
        disk_available_kibibyte: Some(DEFAULT_DISK_FLOOR_KIBIBYTE * 2),
        disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
        ccache_size_kibibyte: Some(1),
        ccache_max_kibibyte: Some(2),
    };

    let centrally_observed = analyze_host(
        capacity,
        doctor,
        storage,
        healthy_attestation(),
        "macos",
        corroborated_lane(),
    );
    assert!(centrally_observed.routable);
    assert_eq!(centrally_observed.problem_count, 0);
}

#[test]
fn doctor_probe_parses_json_even_when_doctor_exits_nonzero() {
    let output = Command::new("sh")
        .args([
            "-c",
            "printf '%s' '{\"problems\":[{\"id\":\"stale_vm\"}]}' ; exit 1",
        ])
        .output()
        .expect("sh");
    let probe = doctor_probe_from_output(&output, "ssh");

    assert!(probe.readable);
    assert_eq!(probe.source, "ssh (doctor exit 1)");
    assert_eq!(
        probe
            .digest
            .as_ref()
            .and_then(|digest| digest.get("problems"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        1
    );
}

#[test]
fn storage_probe_flags_disk_floor_and_ccache_limit_mismatch() {
    let output = Command::new("sh")
        .args([
            "-c",
            "printf 'disk_path\\t/Users/ci/VMs\\ndisk_available_kibibyte\\t20342374\\ncache_size_kibibyte\\t29255270\\nmax_cache_size_kibibyte\\t5242880\\n'",
        ])
        .output()
        .expect("sh");
    let probe = storage_probe_from_output(&output, "/fallback");
    let problems = storage_problems(&probe);

    assert!(probe.readable);
    assert_eq!(probe.disk_path, "/Users/ci/VMs");
    assert!(
        problems
            .iter()
            .any(|problem| problem.starts_with("disk_floor_unmet:"))
    );
    assert!(
        problems
            .iter()
            .any(|problem| problem.starts_with("ccache_over_limit:"))
    );
}

#[test]
fn storage_probe_does_not_override_ccache_config_discovery() {
    let script = storage_probe_script("/Volumes/Workshop/VMs");

    assert!(script.contains("ccache --print-stats"));
    assert!(
        !script.contains("CCACHE_DIR="),
        "the default fleet probe must let ccache discover the host's canonical config"
    );
}

#[test]
fn routing_mismatch_reports_idle_linux_pool_for_hosted_merge_group() {
    let inventory = RunnerInventory {
        boundary: None,
        attempts: 1,
        readable: true,
        source: "github".to_owned(),
        runners: vec![RepositoryRunner {
            id: 200,
            name: "pulp-ci-ephemeral-200".to_owned(),
            status: "online".to_owned(),
            busy: false,
            labels: vec![
                "self-hosted".to_owned(),
                "Linux".to_owned(),
                "X64".to_owned(),
                "pulp-host-macpro".to_owned(),
            ],
        }],
    };
    let runs = vec![ActiveRunObservation {
        run_id: 42,
        workflow: "Build and Test".to_owned(),
        head_branch: "gh-readonly-queue/main/pr-7-deadbeef".to_owned(),
        head_sha: Some("deadbeef".to_owned()),
        status: "in_progress".to_owned(),
        created_at: None,
        pull_requests: vec![7],
        url: None,
        jobs: vec![JobObservation {
            name: "Linux (x64) [github-hosted]".to_owned(),
            status: "queued".to_owned(),
            runner_name: None,
            labels: vec!["ubuntu-latest".to_owned()],
        }],
    }];

    let mismatches = detect_routing_mismatches(&runs, &inventory);
    assert_eq!(mismatches.len(), 1);
    assert_eq!(mismatches[0].idle_candidates, ["pulp-ci-ephemeral-200"]);

    let completed = vec![ActiveRunObservation {
        jobs: vec![JobObservation {
            status: "completed".to_owned(),
            ..runs[0].jobs[0].clone()
        }],
        ..runs[0].clone()
    }];
    assert!(detect_routing_mismatches(&completed, &inventory).is_empty());
}

#[cfg(unix)]
#[test]
fn repository_runner_inventory_retains_platform_and_pool_labels() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
printf '%s\n' \
  '{"id":200,"name":"pulp-ci-ephemeral-200","status":"online","busy":false,"labels":[{"name":"self-hosted"},{"name":"Linux"},{"name":"X64"},{"name":"pulp-host-macpro"}]}' \
  '{"id":300,"name":"pulp-intel-metal","status":"offline","busy":false,"labels":[{"name":"self-hosted"},{"name":"macOS"},{"name":"X64"},{"name":"pulp-host-macmini"}]}'
"#,
    );

    let inventory = fetch_repository_runners(&actions, "Generous-Corp/pulp");

    assert!(inventory.readable);
    assert_eq!(inventory.runners.len(), 2);
    assert_eq!(inventory.runners[0].name, "pulp-ci-ephemeral-200");
    assert!(
        inventory.runners[0]
            .labels
            .iter()
            .any(|label| label == "pulp-host-macpro")
    );
    assert_eq!(inventory.runners[1].status, "offline");
    assert!(
        inventory.runners[1]
            .labels
            .iter()
            .any(|label| label == "pulp-host-macmini")
    );
}

#[test]
fn expected_host_config_tracks_active_missing_and_inactive_future_machines() {
    let config: toml::Table = toml::from_str(
        r#"
[runner.fleet.expected_host.macpro]
labels = ["self-hosted", "Linux", "X64", "pulp-host-macpro"]
min_online = 2

[runner.fleet.expected_host.macmini]
labels = ["self-hosted", "macOS", "X64", "pulp-host-macmini"]

[runner.fleet.expected_host.macbook_air]
active = false
labels = ["self-hosted", "Linux", "ARM64", "pulp-host-macbook-air"]
"#,
    )
    .expect("config");
    let expected = parse_expected_hosts(&config).expect("expected hosts");
    let inventory = RunnerInventory {
        boundary: None,
        attempts: 1,
        readable: true,
        source: "github".to_owned(),
        runners: vec![RepositoryRunner {
            id: 200,
            name: "pulp-ci-ephemeral-200".to_owned(),
            status: "online".to_owned(),
            busy: false,
            labels: vec![
                "self-hosted".to_owned(),
                "Linux".to_owned(),
                "X64".to_owned(),
                "pulp-host-macpro".to_owned(),
            ],
        }],
    };

    let statuses = assess_expected_hosts(&expected, &inventory, "macos");

    let macpro = statuses.iter().find(|host| host.name == "macpro").unwrap();
    assert_eq!(macpro.online, 1);
    assert!(macpro.problem.is_some(), "two MacPro runners are expected");
    let macmini = statuses.iter().find(|host| host.name == "macmini").unwrap();
    assert_eq!(macmini.matching_runners.len(), 0);
    assert!(
        macmini.problem.is_some(),
        "unfinished active host must alert"
    );
    let future = statuses
        .iter()
        .find(|host| host.name == "macbook_air")
        .unwrap();
    assert!(!future.active);
    assert!(
        future.problem.is_none(),
        "inactive future host is inventory only"
    );

    let unreadable = assess_expected_hosts(
        &expected,
        &RunnerInventory {
            boundary: None,
            attempts: 1,
            readable: false,
            source: "rate limited".to_owned(),
            runners: Vec::new(),
        },
        "macos",
    );
    assert_eq!(
        unreadable
            .iter()
            .find(|host| host.name == "macmini")
            .and_then(|host| host.problem.as_deref()),
        Some("runner_inventory_unreadable")
    );
}

#[test]
fn expected_host_config_rejects_missing_or_malformed_labels() {
    let missing: toml::Table =
        toml::from_str("[runner.fleet.expected_host.macmini]\nactive = true\n").unwrap();
    assert!(parse_expected_hosts(&missing).is_err());
    let malformed: toml::Table =
        toml::from_str("[runner.fleet.expected_host.macmini]\nlabels = [\"self-hosted\", 3]\n")
            .unwrap();
    assert!(parse_expected_hosts(&malformed).is_err());
}

#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    let path = temp.path().join("gh");
    crate::test_support::write_executable_script(&path, &format!("#!/bin/sh\nset -eu\n{body}\n"));
    GitHubActions::new(temp.path()).with_gh_binary_for_tests(path)
}

#[cfg(unix)]
#[test]
fn transport_keeps_optional_runs_and_finds_queued_job_inside_in_progress_run() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs?status=in_progress"*)
printf '%s' '{"workflow_runs":[
  {"id":10,"name":"Required","head_branch":"gh-readonly-queue/main/pr-11-a","head_sha":"aaa","status":"in_progress","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":11}]},
  {"id":20,"name":"Examples","head_branch":"feature/demo","head_sha":"bbb","status":"in_progress","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":22}]},
  {"id":30,"name":"Scheduled maintenance","head_branch":null,"head_sha":"ccc","status":"in_progress","created_at":"2026-07-26T00:00:00Z","pull_requests":[]}
]}' ;;
  *"actions/runs?status=queued"*) printf '%s' '{"workflow_runs":[]}' ;;
  *"actions/runs/10/jobs"*)
printf '%s' '{"jobs":[{"name":"macOS required","status":"queued","runner_name":"","labels":["self-hosted","pulp-build-m5"]}]}' ;;
  *"actions/runs/20/jobs"*)
printf '%s' '{"jobs":[{"name":"Validate examples (macOS)","status":"in_progress","runner_name":"pulp-vm-m1-01","labels":["self-hosted","pulp-build-m1"]}]}' ;;
  *"actions/runs/30/jobs"*)
printf '%s' '{"jobs":[{"name":"Maintenance","status":"in_progress","runner_name":"pulp-vm-m5-01","labels":["self-hosted","pulp-build-m5"]}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let observed = fetch_observed_workflow_runs(&actions, "owner/repo", 100).expect("observe runs");
    assert_eq!(observed.runs.len(), 3);
    assert_eq!(
        observed
            .runs
            .iter()
            .find(|run| run.run_id == 30)
            .expect("null-head run retained")
            .head_branch,
        ""
    );
    let queued = queued_macos_summary(&observed.runs, "macos");
    assert_eq!(queued.count, 1);
}

#[cfg(unix)]
#[test]
fn transport_paginates_merge_queue_instead_of_misclassifying_followers() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"cursor=NEXT"*)
printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":100,"enqueuedAt":"2026-07-26T00:00:00Z","headCommit":{"oid":"bbb"},"pullRequest":{"number":222}}],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}' ;;
  *)
printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":0,"enqueuedAt":"2026-07-26T00:00:00Z","headCommit":{"oid":"aaa"},"pullRequest":{"number":111}}],"pageInfo":{"hasNextPage":true,"endCursor":"NEXT"}}}}}}' ;;
esac
"#,
    );
    let (entries, truncated) =
        fetch_merge_queue_entries(&actions, "owner", "repo", "main", 5).expect("queue");
    assert!(!truncated);
    assert_eq!(
        entries.iter().map(|entry| entry.pr).collect::<Vec<_>>(),
        [111, 222]
    );
}

#[cfg(unix)]
#[test]
fn release_skip_is_per_commit_and_does_not_hide_unskipped_source() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"commits/docs"*) printf '%s' '{"files":[{"filename":"docs/readme.md"}]}' ;;
  *"commits/source-old"*) printf '%s' '{"files":[{"filename":"src/old.rs"}]}' ;;
  *"commits/source"*) printf '%s' '{"files":[{"filename":"src/lib.rs"}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let skipped_source = serde_json::json!({
        "commits": [{
            "sha": "source",
            "commit": {"message": "bot\n\nRelease: skip reason=\"generated release\""}
        }]
    });
    assert_eq!(
        count_releasable_commits(&actions, "owner/repo", &skipped_source, 1)
            .expect("complete comparison"),
        ReleasableCommitSummary {
            count: 0,
            truncated: false,
            oldest_committed_at: None,
        }
    );
    let mixed = serde_json::json!({
        "commits": [
            {
                "sha": "source",
                "commit": {
                    "message": "feat: source behavior",
                    "committer": {"date": "2026-07-25T12:00:00Z"}
                }
            },
            {
                "sha": "docs",
                "commit": {"message": "docs\n\nRelease: skip reason=\"generated release\""}
            },
            {
                "sha": "source-old",
                "commit": {
                    "message": "feat: older source behavior",
                    "committer": {"date": "2026-07-24T12:00:00Z"}
                }
            },
        ]
    });
    assert_eq!(
        count_releasable_commits(&actions, "owner/repo", &mixed, 3).expect("complete comparison"),
        ReleasableCommitSummary {
            count: 2,
            truncated: false,
            oldest_committed_at: Some("2026-07-24T12:00:00Z".to_owned()),
        }
    );
}

#[test]
fn release_skip_grammar_matches_case_insensitive_workflow_guard() {
    for message in [
        "bot\n\nrelease: skip reason=\"generated release\"",
        "bot\n\nReLeAsE: SkIp reason=\"generated release\"\nReviewed-by: Bot <bot@example.com>",
        "bot\n\nRelease: skip\n continuation text\n# trailing comment",
    ] {
        assert!(release_is_skipped(message), "{message}");
    }
    for message in [
        "Release: skip",
        "bot\n\nRelease: skip reason=\"quoted prose\"\n\nMore prose follows.",
        "bot\n\nRelease: skip reason=\"not final\"\n\nReviewed-by: Bot <bot@example.com>",
        "bot\n\nRelease: skip\nnot a trailer",
        "bot\n\nRelease:",
        "bot\n\nRelease: ship",
        "bot\n\nRelease-notes: skip",
        "bot\n\nRelease skip",
        "bot\n\n  # indented comment prevents trailer parsing\nRelease: skip",
    ] {
        assert!(!release_is_skipped(message), "{message}");
    }
}

#[cfg(unix)]
#[test]
fn release_commit_detail_lookups_are_bounded_and_fail_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            "printf x >> '{}'\nprintf '%s' '{{\"files\":[{{\"filename\":\"docs/readme.md\"}}]}}'",
            calls.display()
        ),
    );
    let commits = (0..=MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK)
        .map(|index| {
            serde_json::json!({
                "sha": format!("sha-{index}"),
                "commit": {"message": "docs"}
            })
        })
        .collect::<Vec<_>>();
    let comparison = serde_json::json!({"commits": commits});
    assert_eq!(
        count_releasable_commits(
            &actions,
            "owner/repo",
            &comparison,
            u64::try_from(MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK + 1).expect("count")
        )
        .expect("bounded comparison"),
        ReleasableCommitSummary {
            count: 1,
            truncated: true,
            oldest_committed_at: None,
        }
    );
    assert_eq!(
        fs::read_to_string(calls).expect("calls").len(),
        MAX_RELEASE_COMMIT_LOOKUPS_PER_TICK
    );
}

#[cfg(unix)]
#[test]
fn only_latest_release_404_is_treated_as_no_releases() {
    let temp = tempfile::tempdir().expect("temp");
    let no_release = fake_gh(&temp, "echo 'HTTP 404 Not Found' >&2; exit 1");
    let probe = inspect_release_liveness(&no_release, "owner/repo", "main", 86_400)
        .expect("missing latest release is healthy");
    assert!(probe.readable);
    assert_eq!(probe.source, "github (no releases)");
    assert!(probe.report.is_none());

    let temp = tempfile::tempdir().expect("temp");
    let missing_compare = fake_gh(
        &temp,
        r#"
case "$*" in
  *"releases/latest"*) printf '%s' '{"tag_name":"v1.0.0","published_at":"2026-07-26T00:00:00Z"}' ;;
  *"/compare/"*) echo "HTTP 404 Not Found" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let Err(error) = inspect_release_liveness(&missing_compare, "owner/repo", "main", 86_400)
    else {
        panic!("missing comparison should be unhealthy");
    };
    assert!(error.contains("compare latest release to main failed"));
    assert!(error.contains("404 Not Found"));
}

#[cfg(unix)]
#[test]
fn optional_release_workflow_failure_is_nonfatal_auxiliary_observation() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"releases/latest"*) printf '%s' '{"tag_name":"v1.0.0","published_at":"2026-07-26T00:00:00Z"}' ;;
  *"/compare/"*) printf '%s' '{"ahead_by":0,"commits":[]}' ;;
  *"/contents/VERSION"*) printf '%s' '{"content":"MC44MC4wCg=="}' ;;
  *"/issues?"*) printf '%s' '[]' ;;
  *"/actions/workflows/auto-release.yml/"*) echo "HTTP 403 Forbidden" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );

    let probe =
        inspect_release_liveness(&actions, "owner/repo", "main", 86_400).expect("release probe");

    assert!(probe.readable);
    assert_eq!(
        probe.reason_codes,
        [ObservationReason::AuxiliaryObservationUnavailable]
    );
    assert!(probe.report.is_some());
}

#[cfg(unix)]
#[test]
fn check_observations_include_legacy_commit_statuses() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"/check-runs"*) printf '%s' '{"check_runs":[]}' ;;
  *"/statuses"*) printf '%s' '[{"context":"legacy-ci","state":"success","created_at":"2026-07-26T00:00:00Z","updated_at":"2026-07-26T00:01:00Z"},{"context":"legacy-pending","state":"pending","created_at":"2026-07-26T00:02:00Z"}]' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let (checks, truncated) =
        fetch_check_observations(&actions, "owner/repo", "abc").expect("checks");
    assert!(!truncated);
    assert_eq!(checks.len(), 2);
    assert_eq!(checks[0].name, "legacy-ci");
    assert_eq!(checks[0].status, "completed");
    assert_eq!(checks[0].conclusion.as_deref(), Some("success"));
    assert_eq!(
        checks[0].started_at.as_deref(),
        Some("2026-07-26T00:01:00Z")
    );
    assert_eq!(checks[1].status, "in_progress");
    assert_eq!(checks[1].conclusion, None);
}

#[cfg(unix)]
#[test]
fn durable_snapshot_detects_open_pr_whose_auto_merge_was_cleared() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            "case \"$*\" in\n  *isInMergeQueue*) printf '%s' '{{\"data\":{{\"repository\":{{\"pr11\":{{\"isInMergeQueue\":false}}}}}}}}' ;;\n  *) printf x >> '{}'\nprintf '%s' '{{\"state\":\"open\",\"base\":{{\"ref\":\"main\"}},\"auto_merge\":null}}' ;;\nesac",
            calls.display()
        ),
    );
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z"}]}"#,
    )
    .expect("snapshot");
    let (cleared, truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile");
    assert_eq!(cleared, [11]);
    assert!(!truncated);
    let (still_cleared, still_truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile again");
    assert_eq!(still_cleared, [11]);
    assert!(!still_truncated);
    assert_eq!(fs::read_to_string(calls).expect("calls"), "xx");
}

/// A PR ejected and re-enqueued between the queue snapshot and the REST read
/// reports REST `auto_merge: null` (GitHub consumes the request on enqueue)
/// while GraphQL says it is in the queue. That is not a cleared enrollment.
#[cfg(unix)]
#[test]
fn re_enqueued_pr_with_null_rest_auto_merge_is_not_reported_cleared() {
    let temp = tempfile::tempdir().expect("temp");
    let config = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    let gh = temp.path().join("gh");
    crate::test_support::write_executable_script(
        &gh,
        &format!(
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{}'
case "$*" in
  *isInMergeQueue*) printf '%s' '{{"data":{{"repository":{{"pr11":{{"isInMergeQueue":true}}}}}}}}' ;;
  *"repos/owner/repo/pulls/11"*) printf '%s' '{{"state":"open","base":{{"ref":"main"}},"auto_merge":null}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            temp.path().join("calls").display()
        ),
    );
    let actions =
        GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(gh);
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z"}]}"#,
    )
    .expect("snapshot");

    let (cleared, truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile");

    assert!(
        cleared.is_empty(),
        "re-enqueued PR falsely reported cleared"
    );
    assert!(!truncated);
    let calls = fs::read_to_string(temp.path().join("calls")).expect("calls");
    // Control: both reads actually happened, so the verdict rests on the
    // GraphQL membership answer rather than on a skipped lookup.
    assert!(calls.contains("repos/owner/repo/pulls/11"), "{calls}");
    assert!(calls.contains("pr11:pullRequest(number:11)"), "{calls}");
    assert!(calls.contains("-f owner=owner -f name=repo"), "{calls}");
    let persisted: Value =
        serde_json::from_str(&fs::read_to_string(path).expect("snapshot")).expect("JSON");
    assert_eq!(persisted["entries"][0]["auto_merge_cleared"], false);
}

/// A tick with several REST-null candidates spends one REST read each plus a
/// single batched membership read, and never re-queries an entry already
/// recorded as cleared.
#[cfg(unix)]
#[test]
fn null_auto_merge_membership_rechecks_are_batched_once_per_tick() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "$*" in
  *isInMergeQueue*) printf '%s' '{{"data":{{"repository":{{"pr1":{{"isInMergeQueue":false}},"pr2":{{"isInMergeQueue":true}},"pr3":{{"isInMergeQueue":false}}}}}}}}' ;;
  *) printf '%s' '{{"state":"open","base":{{"ref":"main"}},"auto_merge":null}}' ;;
esac"#,
            calls.display()
        ),
    );
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[
          {"pr":1,"head_sha":"a","observed_at":"2026-07-26T00:00:00Z"},
          {"pr":2,"head_sha":"b","observed_at":"2026-07-26T00:00:00Z"},
          {"pr":3,"head_sha":"c","observed_at":"2026-07-26T00:00:00Z"},
          {"pr":4,"head_sha":"d","observed_at":"2026-07-26T00:00:00Z","auto_merge_cleared":true}
        ]}"#,
    )
    .expect("snapshot");

    let (cleared, truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile");

    assert_eq!(cleared, [1, 3, 4]);
    assert!(!truncated);
    let calls = fs::read_to_string(calls).expect("calls");
    let rest = calls
        .lines()
        .filter(|line| line.contains("/pulls/"))
        .count();
    let graphql = calls
        .lines()
        .filter(|line| line.contains("isInMergeQueue"))
        .collect::<Vec<_>>();
    assert_eq!(rest, 4, "{calls}");
    assert_eq!(
        graphql.len(),
        1,
        "one batched membership read per tick: {calls}"
    );
    assert!(graphql[0].contains("pr1:pullRequest(number:1)"), "{calls}");
    assert!(graphql[0].contains("pr3:pullRequest(number:3)"), "{calls}");
    assert!(
        !graphql[0].contains("number:4"),
        "an entry already recorded as cleared must not be re-queried: {calls}"
    );
    assert_eq!(calls.lines().count(), 5, "{calls}");
}

/// An unreadable membership re-check fails the observation rather than
/// guessing "cleared".
#[cfg(unix)]
#[test]
fn unreadable_membership_recheck_fails_closed() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"case "$*" in
  *isInMergeQueue*) echo 'HTTP 502' >&2; exit 1 ;;
  *) printf '%s' '{"state":"open","base":{"ref":"main"},"auto_merge":null}' ;;
esac"#,
    );
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z"}]}"#,
    )
    .expect("snapshot");
    let error =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect_err("unreadable membership must not read as cleared");
    assert!(error.contains("membership"), "{error}");
}

#[cfg(unix)]
#[test]
fn truncated_queue_snapshot_retains_unseen_enrollment_without_alert_or_lookup() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo unexpected >&2; exit 2");
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":501,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z","auto_merge_cleared":true}]}"#,
    )
    .expect("snapshot");

    let (cleared, enrollment_truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], false)
            .expect("partial reconciliation");

    assert!(cleared.is_empty());
    assert!(!enrollment_truncated);
    let persisted: Value =
        serde_json::from_str(&fs::read_to_string(path).expect("snapshot")).expect("JSON");
    assert_eq!(persisted["entries"][0]["pr"], 501);
}

#[cfg(unix)]
#[test]
fn enrollment_snapshot_preserves_authoritative_reentry_timestamp() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo unexpected >&2; exit 2");
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-25T00:00:00Z"}]}"#,
    )
    .expect("snapshot");
    let mut entries = vec![crate::merge_queue_liveness::MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("aaa".to_owned()),
        enqueued_at: Some("2026-07-26T12:00:00Z".to_owned()),
        head_observed_at: None,
    }];

    reconcile_enrollment_snapshot(
        &actions,
        "owner/repo",
        "main",
        temp.path(),
        &mut entries,
        true,
    )
    .expect("reconcile");

    assert_eq!(
        entries[0].enqueued_at.as_deref(),
        Some("2026-07-26T12:00:00Z")
    );
    let persisted: Value =
        serde_json::from_str(&fs::read_to_string(path).expect("persisted enrollment snapshot"))
            .expect("snapshot JSON");
    assert_eq!(
        persisted["entries"][0]["head_observed_at"],
        entries[0]
            .head_observed_at
            .as_deref()
            .expect("head observation timestamp")
    );
}

#[cfg(unix)]
#[test]
fn enrollment_snapshot_resets_head_age_when_exact_head_changes() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo unexpected >&2; exit 2");
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"old","observed_at":"2026-07-25T00:00:00Z","head_observed_at":"2026-07-25T00:01:00Z"}]}"#,
    )
    .expect("snapshot");
    let mut entries = vec![crate::merge_queue_liveness::MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("new".to_owned()),
        enqueued_at: Some("2026-07-25T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];

    reconcile_enrollment_snapshot(
        &actions,
        "owner/repo",
        "main",
        temp.path(),
        &mut entries,
        true,
    )
    .expect("reconcile");

    assert_ne!(
        entries[0].head_observed_at.as_deref(),
        Some("2026-07-25T00:01:00Z")
    );
}

#[cfg(unix)]
#[test]
fn enrollment_snapshot_preserves_head_age_for_same_enrollment_and_head() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo unexpected >&2; exit 2");
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"same","enqueued_at":"2026-07-25T00:00:00Z","head_observed_at":"2026-07-25T00:01:00Z"}]}"#,
    )
    .expect("snapshot");
    let mut entries = vec![crate::merge_queue_liveness::MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("same".to_owned()),
        enqueued_at: Some("2026-07-25T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];

    reconcile_enrollment_snapshot(
        &actions,
        "owner/repo",
        "main",
        temp.path(),
        &mut entries,
        true,
    )
    .expect("reconcile");

    assert_eq!(
        entries[0].head_observed_at.as_deref(),
        Some("2026-07-25T00:01:00Z")
    );
}

#[cfg(unix)]
#[test]
fn enrollment_snapshot_resets_head_age_when_enrollment_changes() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "echo unexpected >&2; exit 2");
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"same","observed_at":"2026-07-25T00:00:00Z","head_observed_at":"2026-07-25T00:01:00Z"}]}"#,
    )
    .expect("snapshot");
    let mut entries = vec![crate::merge_queue_liveness::MergeQueueEntry {
        pr: 11,
        position: 0,
        head_sha: Some("same".to_owned()),
        enqueued_at: Some("2026-07-26T00:00:00Z".to_owned()),
        head_observed_at: None,
    }];

    reconcile_enrollment_snapshot(
        &actions,
        "owner/repo",
        "main",
        temp.path(),
        &mut entries,
        true,
    )
    .expect("reconcile");

    assert_ne!(
        entries[0].head_observed_at.as_deref(),
        Some("2026-07-25T00:01:00Z")
    );
}

#[cfg(unix)]
#[test]
fn retained_enrollment_alert_is_revalidated_and_clears_when_pr_closes() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"state":"closed","base":{"ref":"main"},"auto_merge":null}'"#,
    );
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z","auto_merge_cleared":true}]}"#,
    )
    .expect("snapshot");
    let (cleared, truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile");
    assert!(cleared.is_empty());
    assert!(!truncated);
}

#[cfg(unix)]
#[test]
fn retargeted_pr_is_not_reported_as_cleared_enrollment() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"printf '%s' '{"state":"open","base":{"ref":"release"},"auto_merge":null}'"#,
    );
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(
        &path,
        r#"{"entries":[{"pr":11,"head_sha":"aaa","observed_at":"2026-07-26T00:00:00Z"}]}"#,
    )
    .expect("snapshot");
    let (cleared, truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile");
    assert!(cleared.is_empty());
    assert!(!truncated);
}

#[cfg(unix)]
#[test]
fn malformed_enrollment_snapshot_fails_closed_without_overwrite() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, "exit 99");
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    fs::write(&path, "not json").expect("snapshot");
    let error =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect_err("corrupt history must be visible");
    assert!(error.contains("parse fleet enrollment snapshot failed"));
    assert_eq!(fs::read_to_string(path).expect("snapshot"), "not json");
}

#[cfg(unix)]
#[test]
fn enrollment_reconciliation_has_a_fixed_per_tick_api_budget() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            "printf x >> '{}'\nprintf '%s' '{{\"state\":\"open\",\"base\":{{\"ref\":\"main\"}},\"auto_merge\":{{}}}}'",
            calls.display()
        ),
    );
    let path = enrollment_snapshot_path(temp.path(), "owner/repo", "main");
    fs::create_dir_all(path.parent().expect("parent")).expect("state dir");
    let entries = (1..=MAX_ENROLLMENT_LOOKUPS_PER_TICK + 1)
        .map(|pr| {
            serde_json::json!({
                "pr": pr,
                "head_sha": null,
                "observed_at": "2026-07-26T00:00:00Z"
            })
        })
        .collect::<Vec<_>>();
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"entries": entries})).expect("snapshot JSON"),
    )
    .expect("snapshot");
    let (cleared, truncated) =
        reconcile_enrollment_snapshot(&actions, "owner/repo", "main", temp.path(), &mut [], true)
            .expect("reconcile");
    assert!(cleared.is_empty());
    assert!(truncated);
    assert_eq!(
        fs::read_to_string(calls).expect("calls").len(),
        MAX_ENROLLMENT_LOOKUPS_PER_TICK
    );
}

#[test]
fn observation_failures_have_stable_auth_and_rate_limit_reasons() {
    assert_eq!(
        classify_observation_error("HTTP 403: API rate limit exceeded"),
        ObservationReason::GitHubRateLimited
    );
    assert_eq!(
        classify_observation_error("HTTP 401: Bad credentials"),
        ObservationReason::GitHubAuthFailed
    );
    let rate_error = parse_merge_queue_entries(&serde_json::json!({
        "data": null,
        "errors": [{
            "message": "Something went wrong while executing your query",
            "extensions": {"type": "RATE_LIMITED"}
        }]
    }))
    .expect_err("GraphQL rate limit must fail");
    assert!(rate_error.contains("RATE_LIMITED"), "{rate_error}");
    assert_eq!(
        classify_observation_error(&rate_error),
        ObservationReason::GitHubRateLimited
    );

    let auth_error = parse_merge_queue_entries(&serde_json::json!({
        "data": null,
        "errors": [{"message": "Resource not accessible by integration"}]
    }))
    .expect_err("GraphQL auth failure must fail");
    assert!(
        auth_error.contains("Resource not accessible by integration"),
        "{auth_error}"
    );
    assert_eq!(
        classify_observation_error(&auth_error),
        ObservationReason::GitHubAuthFailed
    );
}

#[test]
fn enrollment_snapshot_keys_do_not_alias_punctuation_variants() {
    let root = Path::new("/tmp/state");
    assert_ne!(
        enrollment_snapshot_path(root, "foo/bar-baz", "release/x"),
        enrollment_snapshot_path(root, "foo-bar/baz", "release-x")
    );
}

#[test]
fn initial_merge_queue_cursor_is_nullable() {
    assert!(MERGE_QUEUE_QUERY.contains("$cursor:String)"));
    assert!(!MERGE_QUEUE_QUERY.contains("$cursor:String!"));
}

#[test]
fn active_run_selection_is_globally_bounded_and_fair_across_statuses() {
    let in_progress = (0..80)
        .map(|id| serde_json::json!({"id": id}))
        .collect::<Vec<_>>();
    let queued = (100..180)
        .map(|id| serde_json::json!({"id": id}))
        .collect::<Vec<_>>();
    let selected = select_bounded_runs(&[in_progress, queued], 50);
    assert_eq!(selected.len(), 50);
    assert_eq!(
        selected
            .iter()
            .filter(|run| run["id"].as_u64().is_some_and(|id| id < 100))
            .count(),
        25
    );
    assert_eq!(
        selected
            .iter()
            .filter(|run| run["id"].as_u64().is_some_and(|id| id >= 100))
            .count(),
        25
    );
}

#[test]
fn downstream_queued_job_does_not_inherit_in_progress_workflow_age() {
    let job = JobObservation {
        name: "macOS required".to_owned(),
        status: "queued".to_owned(),
        runner_name: None,
        labels: Vec::new(),
    };
    let run = |status: &str| ActiveRunObservation {
        run_id: 1,
        workflow: "Build".to_owned(),
        head_branch: "feature".to_owned(),
        head_sha: None,
        status: status.to_owned(),
        created_at: Some("1970-01-01T00:00:00Z".to_owned()),
        pull_requests: Vec::new(),
        url: None,
        jobs: vec![job.clone()],
    };
    let downstream = queued_macos_summary(&[run("in_progress")], "macos");
    assert_eq!(downstream.count, 1);
    assert_eq!(downstream.oldest_age_secs, None);
    let wholly_queued = queued_macos_summary(&[run("queued")], "macos");
    assert!(wholly_queued.oldest_age_secs.is_some());
}

#[test]
fn release_classification_uses_changed_paths_not_commit_labels() {
    assert!(path_requires_release("src/installer.rs"));
    assert!(!path_requires_release("skills/ci/SKILL.md"));
    assert!(!path_requires_release(".claude-plugin/plugin.json"));
    assert!(!path_requires_release("docs/installer.md"));
    assert!(!path_requires_release("CHANGELOG.md"));
}

#[test]
fn release_classification_accounts_for_both_sides_of_renames() {
    assert!(file_change_requires_release(&serde_json::json!({
        "filename": "docs/removed-source.md",
        "previous_filename": "src/removed.rs"
    })));
    assert!(file_change_requires_release(&serde_json::json!({
        "filename": "src/promoted.rs",
        "previous_filename": "docs/promoted.md"
    })));
    assert!(!file_change_requires_release(&serde_json::json!({
        "filename": "docs/new-name.md",
        "previous_filename": "docs/old-name.md"
    })));
}

#[test]
fn malformed_release_file_changes_fail_closed() {
    assert!(file_change_requires_release(&serde_json::json!({})));
    assert!(file_change_requires_release(&serde_json::json!({
        "filename": "docs/new-name.md",
        "previous_filename": 42
    })));
}

#[test]
fn release_api_paths_encode_custom_tags_and_branch_refs() {
    assert_eq!(
        release_compare_path("owner/repo", "release/v1 + hotfix", "release/1.2"),
        "repos/owner/repo/compare/release%2Fv1%20%2B%20hotfix...release%2F1.2"
    );
    assert_eq!(
        release_workflow_runs_path("owner/repo", "release/1.2 + patch"),
        "repos/owner/repo/actions/workflows/auto-release.yml/runs?branch=release%2F1.2%20%2B%20patch&status=success&per_page=1"
    );
    assert_eq!(
        base_version_path("owner/repo", "release/1.2 + patch"),
        "repos/owner/repo/contents/VERSION?ref=release%2F1.2%20%2B%20patch"
    );
}

/// A readable attestation with nothing wrong, for tests whose subject is some
/// other probe. Constructed rather than parsed so a parser change cannot
/// silently turn an unrelated test's fixture unreadable.
fn healthy_capacity() -> HostCapacity {
    HostCapacity {
        class: "m5".to_owned(),
        ssh: Some("m5".to_owned()),
        cap: 2,
        running: Some(0),
        source: "test".to_owned(),
    }
}

fn healthy_doctor() -> DoctorProbe {
    DoctorProbe {
        readable: true,
        source: "test".to_owned(),
        digest: Some(serde_json::json!({
            "config": {"heartbeat_stale_secs": 900},
            "supervisors": [{
                "runner":"pulp-vm-m5-01",
                "labels":"self-hosted,macOS,ARM64",
                "owner_pid_alive":true,
                "heartbeat_age_secs":5
            }]
        })),
    }
}

fn healthy_storage() -> StorageProbe {
    StorageProbe {
        readable: true,
        source: "test".to_owned(),
        disk_path: "/Users/ci/VMs".to_owned(),
        disk_available_kibibyte: Some(DEFAULT_DISK_FLOOR_KIBIBYTE * 2),
        disk_floor_kibibyte: DEFAULT_DISK_FLOOR_KIBIBYTE,
        ccache_size_kibibyte: Some(1),
        ccache_max_kibibyte: Some(2),
    }
}

fn healthy_attestation() -> AttestationProbe {
    AttestationProbe {
        readable: true,
        source: "test".to_owned(),
        written_at: Some("2026-09-13T18:00:00Z".to_owned()),
        age_secs: Some(30),
        interval_secs: Some(300),
        launchd_readable: Some(true),
        persistent_runner_count: 3,
        ..AttestationProbe::default()
    }
}

fn attestation_output(body: &str) -> Output {
    Command::new("sh")
        .args(["-c", "printf '%s' \"$FIXTURE\""])
        .env("FIXTURE", body)
        .output()
        .expect("sh")
}

fn attestation_fixture(written_at: &str, runners: &Value) -> String {
    serde_json::json!({
        "schema": 1,
        "host": "m5",
        "written_at": written_at,
        "interval_secs": 300,
        "launchd_readable": true,
        "generation": {"writer_sha256": "f657fef51ea2"},
        "persistent_runners": runners,
        "jit_lanes": [],
    })
    .to_string()
}

fn parsed_at(body: &str, now: &str) -> AttestationProbe {
    let now = DateTime::parse_from_rfc3339(now)
        .expect("fixture clock")
        .with_timezone(&Utc);
    attestation_probe_from_output(&attestation_output(body), "ssh", now)
}

#[test]
fn only_a_loaded_crash_looping_runner_is_a_service_fault() {
    let body = attestation_fixture(
        "2026-09-13T18:00:00Z",
        &serde_json::json!([
            {"label":"pulp-preamble-m5","verdict":"broken","loaded":true,
             "crash_loop":true,"runs":8082,"registered":false},
            {"label":"v8builder","verdict":"broken","loaded":false,
             "crash_loop":false,"runs":0,"registered":false},
            // The case the `loaded` half of the predicate exists for: a runner
            // that was crash-looping and has since been unloaded. Its history
            // is still in the artifact; it is no longer failing to serve.
            {"label":"unloaded-looper","verdict":"broken","loaded":false,
             "crash_loop":true,"runs":904,"registered":false},
            // The case the `crash_loop` half exists for. Every other loaded
            // entry here is `verdict:"ok"`, which the broken filter deletes
            // before the predicate is reached — so without this row the
            // conjunct decides nothing any test can observe.
            {"label":"stuck-but-not-looping","verdict":"broken","loaded":true,
             "crash_loop":false,"runs":3,"registered":false},
            {"label":"shipyard.queue-tick","verdict":"ok","loaded":true,
             "crash_loop":false,"runs":12,"registered":true},
        ]),
    );
    let probe = parsed_at(&body, "2026-09-13T18:01:00Z");

    assert!(probe.readable);
    let problems = attestation_problems(&probe);
    assert_eq!(
        problems.len(),
        1,
        "exactly one runner is failing service: {problems:?}"
    );
    assert!(
        problems[0].contains("pulp-preamble-m5"),
        "the raised problem must name the looping runner: {problems:?}"
    );
    assert!(
        !problems[0].contains("unloaded-looper"),
        "an unloaded runner is not failing to serve, whatever its history"
    );
    assert!(
        !problems[0].contains("stuck-but-not-looping"),
        "a loaded runner the attester called broken for some reason other than \
         looping is not a crash loop, and this check reports crash loops"
    );
}

/// A `written_at` ahead of our own clock must not read as fresh.
///
/// The age is a signed difference against a one-sided ceiling, so a host whose
/// attester stamps local wall-clock time as UTC reports a negative age forever
/// — and its artifact stays "fresh" long after the writer dies.
#[test]
fn an_attestation_stamped_in_the_future_cannot_be_aged() {
    let body = attestation_fixture(
        "2026-09-13T19:00:00Z",
        &serde_json::json!([
            {"label":"pulp-preamble-m5","verdict":"broken","loaded":true,
             "crash_loop":true,"runs":8082,"registered":false},
        ]),
    );
    let probe = parsed_at(&body, "2026-09-13T18:00:00Z");

    assert!(
        !probe.readable,
        "an artifact written an hour in the future cannot be aged"
    );
    assert_eq!(probe.boundary.as_deref(), Some("parse"));
    assert!(
        probe.source.contains("in the future"),
        "the refusal must name the clock disagreement rather than claim the \
         attester stopped writing: {}",
        probe.source
    );
}

/// Without this the refusal above is indistinguishable from one that rejects
/// every host whose clock is not bit-identical to ours.
#[test]
fn control_a_clock_a_few_seconds_ahead_is_still_readable() {
    let body = attestation_fixture(
        "2026-09-13T18:00:10Z",
        &serde_json::json!([
            {"label":"shipyard.queue-tick","verdict":"ok","loaded":true,
             "crash_loop":false,"runs":12,"registered":true},
        ]),
    );
    let probe = parsed_at(&body, "2026-09-13T18:00:00Z");

    assert!(
        probe.readable,
        "ten seconds of ordinary clock jitter is not a fault: {}",
        probe.source
    );
}

/// A document that never says whether it could read the launchd domain has not
/// reported an empty census — it has reported nothing, and the difference is
/// the whole point of the field.
#[test]
fn an_attestation_that_omits_launchd_readability_is_a_blind_census() {
    let body = serde_json::json!({
        "schema": 1, "host": "m5", "written_at": "2026-09-13T18:00:00Z",
        "interval_secs": 300, "persistent_runners": [], "jit_lanes": [],
    })
    .to_string();
    let probe = parsed_at(&body, "2026-09-13T18:01:00Z");

    assert!(
        !probe.readable,
        "a missing launchd_readable must not be read as true"
    );
    assert_eq!(probe.boundary.as_deref(), Some("parse"));
    assert_eq!(probe.launchd_readable, None);
}

/// A plist that is installed but not loaded spawns nothing, so it cannot be
/// failing to serve. The fleet carries nine such entries from a lane
/// migration; raising on `verdict == "broken"` alone would report ten faults
/// where one exists and train the reader to ignore the check.
#[test]
fn control_dormant_broken_runners_raise_nothing_but_are_still_counted() {
    let body = attestation_fixture(
        "2026-09-13T18:00:00Z",
        &serde_json::json!([
            {"label":"studio-01","verdict":"broken","loaded":false,
             "crash_loop":false,"runs":0,"registered":false},
            {"label":"studio-02","verdict":"broken","loaded":false,
             "crash_loop":false,"runs":0,"registered":false},
        ]),
    );
    let probe = parsed_at(&body, "2026-09-13T18:01:00Z");

    assert!(attestation_problems(&probe).is_empty());
    // Without this the empty verdict above is ambiguous: a parser that read no
    // runners at all would also raise nothing.
    assert_eq!(
        probe.persistent_runner_count, 2,
        "the probe must report what it examined"
    );
    assert_eq!(probe.broken.len(), 2, "both entries were seen and judged");
}

#[test]
fn an_attestation_past_its_staleness_ceiling_is_unreadable_not_healthy() {
    let body = attestation_fixture("2026-09-13T18:00:00Z", &serde_json::json!([]));
    // 900s against a 300s cadence: the writer stopped.
    let probe = parsed_at(&body, "2026-09-13T18:15:00Z");

    assert!(!probe.readable);
    assert_eq!(probe.boundary.as_deref(), Some("transport"));
    assert_eq!(probe.age_secs, Some(900));
    assert_eq!(
        attestation_problems(&probe).len(),
        1,
        "a guard that stopped writing is itself the finding"
    );
}

/// The same bytes, read inside the cadence, must pass — otherwise the test
/// above would also pass against a parser that called everything stale.
#[test]
fn control_the_same_attestation_read_within_its_cadence_is_readable() {
    let body = attestation_fixture("2026-09-13T18:00:00Z", &serde_json::json!([]));
    let probe = parsed_at(&body, "2026-09-13T18:04:00Z");

    assert!(probe.readable, "{}", probe.source);
    assert!(probe.boundary.is_none());
    assert!(attestation_problems(&probe).is_empty());
}

#[test]
fn a_non_json_payload_is_a_parse_boundary_not_an_empty_census() {
    // A truncated or half-written artifact must not read as a host with no
    // runners; that is the exact shape of the failure this check exists for.
    let probe = parsed_at("{\"schema\": 1, \"persistent_run", "2026-09-13T18:01:00Z");

    assert!(!probe.readable);
    assert_eq!(probe.boundary.as_deref(), Some("parse"));
    // The boundary alone does not discriminate: a document that parses but
    // carries no written_at also refuses as `parse`. Name the reason, or this
    // test passes whether or not the JSON check runs at all.
    assert!(
        probe.source.contains("is not JSON"),
        "the refusal must say the payload did not parse: {}",
        probe.source
    );
    assert_eq!(probe.persistent_runner_count, 0);
    assert_eq!(attestation_problems(&probe).len(), 1);
}

#[test]
fn an_attestation_without_a_timestamp_cannot_be_aged() {
    let body = serde_json::json!({
        "schema": 1, "interval_secs": 300, "launchd_readable": true,
        "persistent_runners": [], "jit_lanes": [],
    })
    .to_string();
    let probe = parsed_at(&body, "2026-09-13T18:00:00Z");

    assert!(!probe.readable);
    assert_eq!(probe.boundary.as_deref(), Some("parse"));
}

/// A `LaunchAgent` lives in the per-user GUI domain, so the attester reports
/// when it could not enumerate it. That is a blind census, not an empty one —
/// mapping it to `absent` would report every runner as gone.
#[test]
fn an_unreadable_launchd_domain_is_a_scope_boundary_not_an_absence() {
    let body = serde_json::json!({
        "schema": 1, "written_at": "2026-09-13T18:00:00Z", "interval_secs": 300,
        "launchd_readable": false, "persistent_runners": [], "jit_lanes": [],
    })
    .to_string();
    let probe = parsed_at(&body, "2026-09-13T18:01:00Z");

    assert!(!probe.readable);
    assert_eq!(probe.boundary.as_deref(), Some("scope"));
    assert_eq!(probe.launchd_readable, Some(false));
}

#[test]
fn a_missing_artifact_is_unreadable_rather_than_a_clean_host() {
    let output = Command::new("sh")
        .args(["-c", "exit 66"])
        .output()
        .expect("sh");
    let now = DateTime::parse_from_rfc3339("2026-09-13T18:00:00Z")
        .expect("fixture clock")
        .with_timezone(&Utc);
    let probe = attestation_probe_from_output(&output, "ssh", now);

    assert!(!probe.readable);
    assert_eq!(probe.boundary.as_deref(), Some("transport"));
    assert_eq!(attestation_problems(&probe).len(), 1);
}

/// The artifact lives under the attester's own home, which differs per host,
/// so the path must expand on the far side. `shlex_quote` single-quotes the
/// whole script, which kills `~` but leaves `"$HOME"` for the remote `sh -c`.
#[cfg(unix)]
#[test]
fn the_probe_script_expands_home_on_the_host_it_runs_on() {
    let script = attestation_probe_script();
    assert!(
        !script.contains('~'),
        "a literal ~ would not expand: {script}"
    );

    let home = tempfile::tempdir().expect("tempdir");
    let state = home.path().join(".tartci/state");
    std::fs::create_dir_all(&state).expect("state dir");
    let body = attestation_fixture("2026-09-13T18:00:00Z", &serde_json::json!([]));
    std::fs::write(state.join("host-attestation.json"), &body).expect("fixture");

    // Quoted exactly as the ssh path quotes it, so the test fails if quoting
    // ever swallows the expansion.
    let wrapped = format!("sh -c {}", shlex_quote(&script));
    let output = Command::new("sh")
        .args(["-c", &wrapped])
        .env("HOME", home.path())
        .output()
        .expect("sh");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let now = DateTime::parse_from_rfc3339("2026-09-13T18:01:00Z")
        .expect("fixture clock")
        .with_timezone(&Utc);
    let probe = attestation_probe_from_output(&output, "ssh", now);
    assert!(probe.readable, "{}", probe.source);
    assert_eq!(probe.writer_sha256.as_deref(), Some("f657fef51ea2"));
}

/// A finding that renders nowhere is not a check. This asserts the crash-loop
/// verdict survives all the way to both surfaces the operator actually reads.
#[test]
fn an_unreadable_attestation_makes_a_host_unroutable() {
    // `routable` does not test `attestation.readable` directly; it relies on the
    // unreadable arm of attestation_problems raising. This pins that coupling,
    // so removing the arm cannot quietly route work to a host nobody can see.
    let blind = AttestationProbe {
        readable: false,
        boundary: Some("transport".to_owned()),
        source: "ssh attestation exit 66".to_owned(),
        ..AttestationProbe::default()
    };
    let host = analyze_host(
        healthy_capacity(),
        healthy_doctor(),
        healthy_storage(),
        blind,
        FLEET_LANE_TARGET,
        corroborated_lane(),
    );

    assert_eq!(host.problem_count, 1, "the blind read must be a problem");
    assert!(!host.routable, "a host nobody can observe is not routable");

    // Control: the identical host with a readable attestation IS routable, so
    // the assertion above cannot be passing for an unrelated reason.
    let seeing = analyze_host(
        healthy_capacity(),
        healthy_doctor(),
        healthy_storage(),
        healthy_attestation(),
        FLEET_LANE_TARGET,
        corroborated_lane(),
    );
    assert_eq!(seeing.problem_count, 0);
    assert!(seeing.routable, "the control host must be routable");
}

#[test]
fn an_attestation_finding_reaches_both_json_surfaces() {
    let body = attestation_fixture(
        "2026-09-13T18:00:00Z",
        &serde_json::json!([
            {"label":"pulp-preamble-m5","verdict":"broken","loaded":true,
             "crash_loop":true,"runs":8082,"registered":false},
        ]),
    );
    let attestation = parsed_at(&body, "2026-09-13T18:01:00Z");
    let host = analyze_host(
        healthy_capacity(),
        healthy_doctor(),
        healthy_storage(),
        attestation,
        FLEET_LANE_TARGET,
        corroborated_lane(),
    );

    assert_eq!(
        host.problem_count, 1,
        "the crash loop must count as a problem"
    );
    assert!(!host.routable, "a host with a looping runner is not clean");

    let mut assessment = wedge_assessment(WedgedQueuedJobs::default());
    assessment.hosts = vec![host];

    let mut command_output = Vec::new();
    render_fleet_assessment(&assessment, true, &mut command_output).expect("command JSON");
    let command: Value = serde_json::from_slice(&command_output).expect("command document");
    let mut watch_output = Vec::new();
    render_fleet_watch_event(&assessment, &mut watch_output).expect("watch JSON");
    let watch: Value = serde_json::from_slice(&watch_output).expect("watch document");

    for (surface, document) in [("command", &command), ("watch", &watch)] {
        let raised = &document["hosts"][0]["attestation_problems"];
        assert_eq!(
            raised.as_array().map_or(0, Vec::len),
            1,
            "the {surface} surface dropped the finding: {document}"
        );
        assert!(
            raised[0]
                .as_str()
                .is_some_and(|problem| problem.contains("pulp-preamble-m5")),
            "the {surface} surface must name the runner: {document}"
        );
        assert_eq!(document["hosts"][0]["attestation"]["readable"], true);
    }

    let mut text_output = Vec::new();
    render_fleet_assessment(&assessment, false, &mut text_output).expect("text");
    let text = String::from_utf8(text_output).expect("utf8");
    assert!(
        text.contains("attestation: runner_crash_loop:pulp-preamble-m5"),
        "the human surface dropped the finding:\n{text}"
    );
}

/// A host whose only complaint is that its own GitHub read did not answer.
fn doctor_reporting(problem: &str) -> DoctorProbe {
    DoctorProbe {
        readable: true,
        source: "test".to_owned(),
        digest: Some(serde_json::json!({
            "config": {"heartbeat_stale_secs": 900},
            "problems": [problem],
            "supervisors": [{
                "runner":"pulp-vm-m1-01",
                "labels":"self-hosted,macOS,ARM64",
                "owner_pid_alive":true,
                "heartbeat_age_secs":5
            }]
        })),
    }
}

fn analyze_with(problem: &str, corroboration: LaneCorroboration) -> HostFleetStatus {
    analyze_host(
        healthy_capacity(),
        doctor_reporting(problem),
        healthy_storage(),
        healthy_attestation(),
        FLEET_LANE_TARGET,
        corroboration,
    )
}

#[test]
fn an_org_scope_gap_the_repo_census_answered_leaves_the_host_routable() {
    // The incident shape: one host's org-scope read times out, the repo scope
    // reads fine seconds later with a busy runner in it, and the fleet reports
    // routable_free=0 against free=4. Capacity was never the thing that failed.
    let host = analyze_with(
        "github_runners_scope_unreadable:organization",
        corroborated_lane(),
    );

    assert!(
        host.routable,
        "a scope nobody could read is not a capacity fact"
    );
    assert_eq!(host.problem_count, 0);
    assert_eq!(host.routing_confidence(), RoutingConfidence::Degraded);
    let gap = host
        .degraded_observations
        .first()
        .expect("the gap must still be named");
    assert!(
        gap.problem.contains("organization"),
        "the degraded reason must name what could not be read: {}",
        gap.problem
    );
    assert_eq!(gap.boundary, ReadBoundary::Unclassified);

    // Control: the same host with nothing unreadable claims no degradation, so
    // the marker cannot be something every host carries.
    let clean = analyze_host(
        healthy_capacity(),
        healthy_doctor(),
        healthy_storage(),
        healthy_attestation(),
        FLEET_LANE_TARGET,
        corroborated_lane(),
    );
    assert_eq!(clean.routing_confidence(), RoutingConfidence::Confirmed);
    assert!(clean.degraded_observations.is_empty());
}

#[test]
fn a_denied_github_read_keeps_its_host_unroutable() {
    // Control for the case above: GitHub answered, and the answer was no. A
    // denial does not self-heal on the next tick, so corroboration elsewhere
    // does not get to wave it through.
    let host = analyze_with(
        "github_unreadable:HTTP 403: Resource not accessible by integration",
        corroborated_lane(),
    );

    assert!(!host.routable, "a denial is a fleet fact, not a gap");
    assert_eq!(host.problem_count, 1);
    assert!(
        host.degraded_observations.is_empty(),
        "a denial must never be demoted to an observation gap"
    );
    assert_eq!(
        host_github_observation_boundary(&Value::from(
            "github_unreadable:HTTP 403: Resource not accessible by integration"
        )),
        Some(ReadBoundary::Denied)
    );
}

#[test]
fn a_rate_limited_read_is_transient_even_though_github_serves_it_as_403() {
    // GitHub answers a rate limit with HTTP 403. Reading the status before the
    // reason would classify the most common transient failure as permanent and
    // stop retrying exactly the case a retry fixes.
    assert_eq!(
        classify_read_boundary("HTTP 403: API rate limit exceeded"),
        ReadBoundary::Transient
    );
    assert_eq!(
        classify_read_boundary("HTTP 403: Resource not accessible by integration"),
        ReadBoundary::Denied
    );
    assert_eq!(
        classify_read_boundary("gh api repos/x/actions/runners timed out after 2700ms"),
        ReadBoundary::Transient
    );
    assert_eq!(
        classify_read_boundary("github_runners_scope_unreadable"),
        ReadBoundary::Unclassified
    );
}

#[test]
fn a_fleet_nobody_could_read_is_still_not_routable() {
    // The guarantee that has to survive the change. With no readable census to
    // corroborate it, an unreadable scope keeps its host unroutable — the
    // demotion is bought by a second reading, never assumed.
    let blind = analyze_with(
        "github_runners_scope_unreadable:organization",
        uncorroborated_lane(),
    );
    assert!(!blind.routable, "an unreadable fleet must fail closed");
    assert_eq!(blind.problem_count, 1);
    assert!(blind.degraded_observations.is_empty());

    // A census that answered but found no online lane runner corroborates
    // nothing either: half the evidence is not the evidence.
    let empty_census = analyze_with(
        "github_runners_scope_unreadable:organization",
        LaneCorroboration {
            inventory_readable: true,
            online_lane_runners: 0,
        },
    );
    assert!(
        !empty_census.routable,
        "a census with no lane runner corroborates nothing"
    );
    assert_eq!(empty_census.problem_count, 1);
}

/// A fake `gh` that answers regardless of the host's machine-global config.
///
/// [`fake_gh`] inherits whatever global `[github.auth]` the machine carries. A
/// `token_command` there is expanded against the working directory, which for a
/// temp dir is not a checkout — so the census fails during credential
/// preparation and the script under test never runs. Loading an empty global
/// layer keeps these cases measuring retry behaviour rather than the host.
#[cfg(unix)]
fn fake_gh_isolated(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    let global = temp.path().join("global-config");
    fs::create_dir_all(&global).expect("global config dir");
    let config = crate::config::LoadedConfig::load(
        Some(global),
        None,
        None,
        crate::config::LocalOverlaySource::None,
    )
    .expect("empty config loads");
    let path = temp.path().join("gh");
    crate::test_support::write_executable_script(&path, &format!("#!/bin/sh\nset -eu\n{body}\n"));
    GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(path)
}

#[cfg(unix)]
#[test]
fn a_transient_census_failure_is_retried_until_it_answers() {
    let temp = tempfile::tempdir().expect("temp");
    let counter = temp.path().join("attempts");
    let actions = fake_gh_isolated(
        &temp,
        &format!(
            r#"
printf 'x' >> {counter}
attempts=$(wc -c < {counter} | tr -d ' ')
if [ "$attempts" -lt 2 ]; then
  echo "HTTP 503: Service Unavailable" >&2
  exit 1
fi
printf '%s\n' '{{"id":1,"name":"pulp-vm-01","status":"online","busy":false,"labels":[{{"name":"self-hosted"}},{{"name":"macOS"}},{{"name":"ARM64"}}]}}'
"#,
            counter = counter.display()
        ),
    );

    let inventory = fetch_repository_runners_with_backoff(
        &actions,
        "Generous-Corp/pulp",
        &[Duration::from_millis(1), Duration::from_millis(1)],
    );

    assert!(
        inventory.readable,
        "a blip must not decide a routability verdict: {}",
        inventory.source
    );
    assert_eq!(inventory.attempts, 2, "the retry must actually have run");
    assert_eq!(inventory.online_lane_runners("macos"), 1);
}

#[cfg(unix)]
#[test]
fn a_denied_census_failure_is_not_retried() {
    // Control for the case above. Re-asking a refusal cannot change it, and
    // the retry would spend the quota a refusal is sometimes caused by.
    let temp = tempfile::tempdir().expect("temp");
    let counter = temp.path().join("attempts");
    let actions = fake_gh_isolated(
        &temp,
        &format!(
            r#"
printf 'x' >> {counter}
echo "HTTP 403: Resource not accessible by integration" >&2
exit 1
"#,
            counter = counter.display()
        ),
    );

    let inventory = fetch_repository_runners_with_backoff(
        &actions,
        "Generous-Corp/pulp",
        &[Duration::from_millis(1), Duration::from_millis(1)],
    );

    assert!(!inventory.readable);
    assert_eq!(inventory.boundary, Some(ReadBoundary::Denied));
    assert_eq!(inventory.attempts, 1, "a denial must not be retried");
    assert_eq!(
        fs::read_to_string(&counter).expect("counter").len(),
        1,
        "the denial burned more than one call"
    );
    assert!(
        inventory.source.contains("denied"),
        "an unreadable census must name its boundary: {}",
        inventory.source
    );
}

fn expected_host(name: &str, labels: &[&str]) -> ExpectedHostConfig {
    ExpectedHostConfig {
        name: name.to_owned(),
        active: true,
        min_online: 1,
        labels: labels.iter().map(|label| (*label).to_owned()).collect(),
    }
}

#[test]
fn a_non_macos_expected_host_does_not_drive_a_macos_verdict() {
    // Both declared machines are unavailable, and neither can serve a macOS
    // ARM64 job: one is Linux, the other an Intel Mac. Reported, not counted.
    let expected = [
        expected_host(
            "macpro",
            &["self-hosted", "Linux", "X64", "pulp-host-macpro"],
        ),
        expected_host(
            "macmini",
            &["self-hosted", "macOS", "X64", "pulp-host-macmini"],
        ),
    ];
    let statuses = assess_expected_hosts(&expected, &wedge_inventory(Vec::new()), "macos");

    assert!(
        statuses.iter().all(|host| host.problem.is_some()),
        "the finding must not be deleted, only demoted"
    );
    assert!(
        statuses.iter().all(|host| !host.serves_target),
        "neither a Linux nor an Intel host serves the macOS lane"
    );
    assert!(
        expected_hosts_needing_attention(&statuses).is_empty(),
        "a lane nobody asked about must not raise the verdict"
    );
}

#[test]
fn a_macos_expected_host_still_drives_the_macos_verdict() {
    // Control: the identical shortfall on a machine that does serve the lane.
    let expected = [expected_host(
        "studio",
        &["self-hosted", "macOS", "ARM64", "pulp-host-studio"],
    )];
    let statuses = assess_expected_hosts(&expected, &wedge_inventory(Vec::new()), "macos");

    assert!(statuses[0].serves_target);
    assert_eq!(
        expected_hosts_needing_attention(&statuses)
            .iter()
            .map(|host| host.name.as_str())
            .collect::<Vec<_>>(),
        vec!["studio"],
        "a host that serves the lane must still raise it"
    );
}

#[test]
fn an_undeclared_platform_is_not_read_as_a_contradiction() {
    // Silence about a platform is not a claim about it. The Apple Silicon gate
    // hosts register no host-identifying platform label, and excluding a host
    // for failing to declare one would quietly drop the lane's own machines.
    assert!(expected_host_serves_target(
        &["self-hosted".to_owned(), "pulp-build-vm".to_owned()],
        "macos"
    ));
    assert!(expected_host_serves_target(
        &["self-hosted".to_owned(), "Linux".to_owned()],
        "linux"
    ));
    assert!(!expected_host_serves_target(
        &["self-hosted".to_owned(), "Linux".to_owned()],
        "macos"
    ));
}

#[test]
fn other_target_hosts_render_in_their_own_section() {
    let mut assessment = wedge_assessment(WedgedQueuedJobs::default());
    assessment.target = "macos".to_owned();
    assessment.expected_hosts = vec![
        ExpectedHostStatus {
            name: "macpro".to_owned(),
            active: true,
            min_online: 2,
            labels: vec!["Linux".to_owned()],
            matching_runners: Vec::new(),
            online: 0,
            idle: 0,
            serves_target: false,
            problem: Some("expected_host_unavailable:online=0 min_online=2".to_owned()),
        },
        ExpectedHostStatus {
            name: "studio".to_owned(),
            active: true,
            min_online: 1,
            labels: vec!["macOS".to_owned(), "ARM64".to_owned()],
            matching_runners: vec!["pulp-vm-01".to_owned()],
            online: 1,
            idle: 1,
            serves_target: true,
            problem: None,
        },
    ];

    let mut text = Vec::new();
    render_fleet_assessment(&assessment, false, &mut text).expect("text");
    let text = String::from_utf8(text).expect("utf8");

    let section = text
        .find("other targets (not counted against target=macos)")
        .expect("the demoted section must be labelled");
    let macpro = text
        .find("name=macpro")
        .expect("macpro must still be printed");
    assert!(
        macpro > section,
        "macpro must sit under the demoted heading:\n{text}"
    );
    assert!(
        text.find("name=studio").expect("studio printed") < section,
        "a lane host must stay above the demoted heading:\n{text}"
    );
}

#[test]
fn a_degraded_routing_verdict_names_its_gap_on_both_surfaces() {
    let mut assessment = wedge_assessment(WedgedQueuedJobs::default());
    assessment.hosts = vec![analyze_with(
        "github_runners_scope_unreadable:organization",
        corroborated_lane(),
    )];
    assessment.routing_degraded_reasons = routing_degraded_reasons(&assessment.hosts);
    assessment.routing_confidence = RoutingConfidence::Degraded;

    let mut text = Vec::new();
    render_fleet_assessment(&assessment, false, &mut text).expect("text");
    let text = String::from_utf8(text).expect("utf8");
    assert!(
        text.contains("routing_confidence=degraded"),
        "the headline must carry the confidence marker:\n{text}"
    );
    assert!(
        text.contains("routing degraded:") && text.contains("organization"),
        "the human surface must name the gap:\n{text}"
    );

    let mut json = Vec::new();
    render_fleet_assessment(&assessment, true, &mut json).expect("json");
    let document: Value = serde_json::from_slice(&json).expect("json parses");
    assert_eq!(document["routing_confidence"], "degraded");
    assert_eq!(document["hosts"][0]["routing_confidence"], "degraded");
    assert_eq!(
        document["hosts"][0]["degraded_observations"][0]["boundary"],
        "unclassified"
    );
    assert!(
        document["routing_degraded_reasons"][0]
            .as_str()
            .is_some_and(|reason| reason.contains("organization")),
        "the JSON surface must name the gap: {document}"
    );
}
