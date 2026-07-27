use super::*;

#[test]
fn assessment_renders_command_and_watch_json_without_round_trip() {
    let assessment = FleetAssessment {
        repo: "owner/repo".to_owned(),
        target: "macos".to_owned(),
        free: 2,
        routable_free_slots: 1,
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
        github_cli: Some("ghapp".to_owned()),
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
        github_cli: None,
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
        "macos",
    );
    assert!(host.routable);
    assert_eq!(host.problem_count, 0);
    assert_eq!(host.supervisor_count, 1);
    assert_eq!(host.github_runner_count, 1);
    assert_eq!(host.stale_vm_count, 0);
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

#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    use std::os::unix::fs::PermissionsExt;

    let path = temp.path().join("gh");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake gh");
    let mut permissions = fs::metadata(&path).expect("fake gh metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod fake gh");
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
        (0, false)
    );
    let mixed = serde_json::json!({
        "commits": [
            {
                "sha": "source",
                "commit": {"message": "feat: source behavior"}
            },
            {
                "sha": "docs",
                "commit": {"message": "docs\n\nRelease: skip reason=\"generated release\""}
            },
        ]
    });
    assert_eq!(
        count_releasable_commits(&actions, "owner/repo", &mixed, 2).expect("complete comparison"),
        (1, false)
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
        (1, true)
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
            "printf x >> '{}'\nprintf '%s' '{{\"state\":\"open\",\"base\":{{\"ref\":\"main\"}},\"auto_merge\":null}}'",
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
