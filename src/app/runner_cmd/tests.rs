use super::*;
use crate::app::fleet_status_cmd::FleetLivenessPolicy;

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
