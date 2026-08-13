use super::*;
use crate::app::fleet_status_cmd::FleetLivenessPolicy;
#[cfg(unix)]
use crate::app::runner_cmd::watch::reap_stale_runs_tick;

#[test]
fn parse_etime_handles_mm_ss() {
    assert_eq!(parse_etime_minutes("45:12"), Some(45));
}

#[test]
fn parse_etime_handles_hh_mm_ss() {
    assert_eq!(parse_etime_minutes("01:30:00"), Some(90));
}

#[test]
fn parse_etime_handles_days() {
    assert_eq!(parse_etime_minutes("2-03:15:00"), Some(2 * 24 * 60 + 195));
}

#[test]
fn parse_etime_rejects_garbage() {
    assert_eq!(parse_etime_minutes("not-a-time"), None);
}

#[test]
fn parse_ps_row_handles_typical_macos_line() {
    let row = parse_ps_pid_etime_command(
        " 12345 01:30:00 /Users/foo/actions-runner/bin/Runner.Worker spawnclient 0 0",
    )
    .expect("row");
    assert_eq!(row.pid, 12345);
    assert_eq!(row.etime_min, 90);
    assert!(
        row.command
            .starts_with("/Users/foo/actions-runner/bin/Runner.Worker")
    );
}

#[test]
fn parse_ps_row_rejects_missing_command() {
    assert!(parse_ps_pid_etime_command("12345 01:30:00").is_none());
}

#[test]
fn parse_ps_row_rejects_non_numeric_pid() {
    assert!(parse_ps_pid_etime_command("abcd 01:30:00 /bin/Runner.Worker").is_none());
}

#[test]
fn parse_ps_row_collapses_multispace_columns() {
    // Real `ps -ax -o pid=,etime=,command=` output pads PID + etime to
    // fixed-width columns separated by runs of spaces, not single spaces.
    // The previous splitn-based parser silently rejected these rows.
    let row = parse_ps_pid_etime_command(
        "  12345    01:30:00    /Users/foo/actions-runner/bin/Runner.Worker spawnclient 0 0",
    )
    .expect("row");
    assert_eq!(row.pid, 12345);
    assert_eq!(row.etime_min, 90);
    assert!(
        row.command
            .starts_with("/Users/foo/actions-runner/bin/Runner.Worker")
    );
}

#[test]
fn parse_ps_row_preserves_command_internal_spaces() {
    // After collapsing the column gaps, internal spaces in the command
    // (e.g. argv tokens) must survive unchanged.
    let row = parse_ps_pid_etime_command(" 1 00:01 /bin/Runner.Worker arg with multiple   spaces")
        .expect("row");
    assert!(row.command.contains("arg with multiple   spaces"));
}

#[test]
fn parse_github_slug_supports_https_and_ssh() {
    assert_eq!(
        parse_github_repo_slug("git@github.com:danielraffel/Shipyard.git"),
        Some("danielraffel/Shipyard".to_owned())
    );
    assert_eq!(
        parse_github_repo_slug("https://github.com/danielraffel/pulp"),
        Some("danielraffel/pulp".to_owned())
    );
    assert_eq!(parse_github_repo_slug("not-a-github-url"), None);
}

#[test]
fn dry_run_overridden_only_respects_fix_flag() {
    assert!(dry_run_overridden_only(true, false));
    assert!(!dry_run_overridden_only(true, true));
}

#[test]
fn cleanup_report_distinguishes_cancellable_and_protected_stale_runs() {
    let settings = WatchdogSettings {
        repo_slug: "owner/repo".to_owned(),
        runner_id: None,
        runner_dir: std::path::PathBuf::from("/tmp/runner"),
        thresholds: WatchdogThresholds::default(),
    };
    let stale = vec![
        StaleQueuedRun {
            run_id: 1,
            workflow: "CI".to_owned(),
            branch: "feature/x".to_owned(),
            queued_for_secs: 10_000,
            url: None,
            cancellation_safe: true,
        },
        StaleQueuedRun {
            run_id: 2,
            workflow: "Release CLI".to_owned(),
            branch: "main".to_owned(),
            queued_for_secs: 10_000,
            url: None,
            cancellation_safe: false,
        },
    ];

    let mut human = Vec::new();
    emit_cleanup_report(&mut human, &settings, &stale, &[], &[], false, false)
        .expect("human report");
    let human = String::from_utf8(human).expect("UTF-8");
    assert!(human.contains("run 1"));
    assert!(human.contains("[cancellable]"));
    assert!(human.contains("run 2"));
    assert!(human.contains("[protected: not cancellable]"));
    assert!(human.contains("cancel the eligible runs"));
    assert!(human.contains("Protected run ids (not cancellable): [2]"));

    let mut json = Vec::new();
    emit_cleanup_report(&mut json, &settings, &stale, &[], &[], true, true).expect("JSON report");
    let value: serde_json::Value = serde_json::from_slice(&json).expect("parse JSON");
    assert_eq!(value["protected_run_ids"], serde_json::json!([2]));
    assert_eq!(value["stale_queued_runs"][1]["cancellation_safe"], false);
}

#[test]
fn fleet_liveness_is_default_on_for_configured_pool_and_has_cadence() {
    let config = config_with(
        "[host_class.m5]\ncap = 2\n[runner.watchdog]\nfleet_liveness_every_ticks = 3\n",
    );
    assert_eq!(
        fleet_liveness_policy(&config),
        FleetLivenessPolicy::MonitorConfiguredPool { every_ticks: 3 }
    );
    assert!(fleet_liveness_due(&config, 0));
    assert!(!fleet_liveness_due(&config, 1));
    assert!(fleet_liveness_due(&config, 3));
}

#[test]
fn fleet_liveness_can_be_delegated_or_has_no_work_without_pool() {
    let delegated =
        config_with("[host_class.m5]\ncap = 2\n[runner.watchdog]\nfleet_liveness = false\n");
    assert_eq!(
        fleet_liveness_policy(&delegated),
        FleetLivenessPolicy::Delegated
    );
    assert!(!fleet_liveness_due(&delegated, 0));
    let delegated_with_invalid_pool = config_with(
        "[host_class.m5]\ncap = \"invalid\"\n[runner.watchdog]\nfleet_liveness = false\n",
    );
    assert_eq!(
        fleet_liveness_policy(&delegated_with_invalid_pool),
        FleetLivenessPolicy::Delegated
    );
    assert!(!fleet_liveness_due(&delegated_with_invalid_pool, 0));
    let no_pool = config_with("[project]\nname = \"x\"\n");
    assert_eq!(
        fleet_liveness_policy(&no_pool),
        FleetLivenessPolicy::NoConfiguredPool
    );
    assert!(!fleet_liveness_due(&no_pool, 0));
}

#[test]
fn bounded_watch_propagates_fleet_failure_without_masking_offline_health() {
    assert_eq!(watch_exit_code(RunnerHealth::Healthy, true), 1);
    assert_eq!(watch_exit_code(RunnerHealth::Stuck, true), 1);
    assert_eq!(watch_exit_code(RunnerHealth::Offline, true), 2);
    assert_eq!(watch_exit_code(RunnerHealth::Healthy, false), 0);
}

fn config_with(body: &str) -> crate::config::LoadedConfig {
    use crate::config::{LoadedConfig, LocalOverlaySource};
    let sandbox = tempfile::TempDir::new().expect("tempdir");
    let project_dir = sandbox.path().join(".shipyard");
    std::fs::create_dir_all(&project_dir).expect("project dir");
    std::fs::write(project_dir.join("config.toml"), body).expect("write config");
    LoadedConfig::load(
        Some(sandbox.path().join("global-missing")),
        Some(sandbox.path().join(".shipyard")),
        None,
        LocalOverlaySource::None,
    )
    .expect("load config")
}

#[test]
fn reaper_thresholds_fall_back_to_built_in_defaults() {
    let config = config_with("[project]\nname = \"x\"\n");
    let thresholds = resolve_reaper_thresholds(&config, None, None);
    assert_eq!(
        thresholds.in_progress_max_min,
        DEFAULT_REAP_IN_PROGRESS_MAX_MIN
    );
    assert_eq!(thresholds.queued_max_min, DEFAULT_REAP_QUEUED_MAX_MIN);
}

#[test]
fn reaper_thresholds_read_from_config() {
    let config = config_with(
        "[runner.watchdog]\nreap_in_progress_max_min = 120\nreap_queued_max_min = 240\n",
    );
    let thresholds = resolve_reaper_thresholds(&config, None, None);
    assert_eq!(thresholds.in_progress_max_min, 120);
    assert_eq!(thresholds.queued_max_min, 240);
}

#[test]
fn reaper_thresholds_flags_win_over_config() {
    let config = config_with(
        "[runner.watchdog]\nreap_in_progress_max_min = 120\nreap_queued_max_min = 240\n",
    );
    let thresholds = resolve_reaper_thresholds(&config, Some(30), Some(60));
    assert_eq!(thresholds.in_progress_max_min, 30);
    assert_eq!(thresholds.queued_max_min, 60);
}

#[cfg(unix)]
#[test]
fn stale_run_reaper_continues_after_individual_cancellation_failure() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let gh = temp.path().join("gh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> '{calls}'
case "$*" in
  *"status=in_progress"*)
    printf '%s' '{{"workflow_runs":[
      {{"id":101,"name":"first","head_branch":"a","event":"pull_request","created_at":"2020-01-01T00:00:00Z","run_started_at":"2020-01-01T00:00:00Z","status":"in_progress"}},
      {{"id":102,"name":"second","head_branch":"b","event":"merge_group","created_at":"2020-01-01T00:00:00Z","run_started_at":"2020-01-01T00:00:00Z","status":"in_progress"}},
      {{"id":103,"name":"Release CLI","head_branch":"main","event":"workflow_dispatch","path":".github/workflows/release-cli.yml","created_at":"2020-01-01T00:00:00Z","run_started_at":"2020-01-01T00:00:00Z","status":"in_progress"}},
      {{"id":104,"name":"ordinary dispatch","head_branch":"main","event":"workflow_dispatch","path":".github/workflows/ci.yml","created_at":"2020-01-01T00:00:00Z","run_started_at":"2020-01-01T00:00:00Z","status":"in_progress"}}
    ]}}' ;;
  *"status=queued"*) printf '%s' '{{"workflow_runs":[]}}' ;;
  *"runs/101/cancel"*) echo "first cancellation rejected" >&2; exit 1 ;;
  *"runs/102/cancel"*) printf '%s' '{{}}' ;;
  *"runs/103/cancel"*|*"runs/104/cancel"*) echo "protected run cancellation attempted" >&2; exit 3 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls = calls.display()
        ),
    )
    .expect("fake gh");
    let mut permissions = std::fs::metadata(&gh)
        .expect("fake gh metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&gh, permissions).expect("chmod fake gh");
    let actions = GitHubActions::new(temp.path()).with_gh_binary_for_tests(gh);
    let settings = WatchdogSettings {
        repo_slug: "owner/repo".to_owned(),
        runner_id: None,
        runner_dir: temp.path().join("runner"),
        thresholds: WatchdogThresholds::default(),
    };
    let mut output = Vec::new();

    let error = reap_stale_runs_tick(
        &actions,
        &settings,
        ReaperThresholds {
            in_progress_max_min: 1,
            queued_max_min: 1,
        },
        false,
        false,
        &mut output,
    )
    .expect_err("one failed cancellation must fail the tick");

    assert!(error.message.contains("run 101"), "{}", error.message);
    let calls = std::fs::read_to_string(calls).expect("calls");
    assert!(calls.contains("runs/101/cancel"), "{calls}");
    assert!(calls.contains("runs/102/cancel"), "{calls}");
    assert!(!calls.contains("runs/103/cancel"), "{calls}");
    assert!(!calls.contains("runs/104/cancel"), "{calls}");
    let output = String::from_utf8(output).expect("UTF-8");
    assert!(output.contains("failed run=101"), "{output}");
    assert!(output.contains("cancelled run=102"), "{output}");
    assert!(output.contains("skipped run=103"), "{output}");
    assert!(output.contains("skipped run=104"), "{output}");
    assert!(
        output.contains("protected event or release workflow"),
        "{output}"
    );
}
