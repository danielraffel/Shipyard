use super::*;

#[allow(
    clippy::unnecessary_wraps,
    reason = "matches the verify callback signature"
)]
fn accept(_: &Path) -> Result<(), String> {
    Ok(())
}

fn refuse(_: &Path) -> Result<(), String> {
    Err("exit 77: not a Developer ID signed Shipyard".to_owned())
}

fn activated_home() -> tempfile::TempDir {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path();
    let executable = executable_path(home);
    fs::create_dir_all(executable.parent().expect("parent")).expect("bundle dirs");
    fs::write(&executable, "#!/bin/sh\nexit 0\n").expect("executable");
    let record = activation_path(home);
    fs::create_dir_all(record.parent().expect("parent")).expect("record dir");
    fs::write(&record, r#"{"schema": 1}"#).expect("record");
    temp
}

fn launchd_job(exe: &str) -> JobFacts {
    JobFacts {
        ppid: 1,
        xpc_service_name: Some("com.danielraffel.shipyard.fleet-reconcile".to_owned()),
        under_launcher: false,
        routing_off: false,
        current_exe: Some(PathBuf::from(exe)),
        pointer_target: Some(PathBuf::from(
            "/h/.local/share/shipyard/auth-generations/a/shipyard",
        )),
        daemon_internal: false,
    }
}

#[test]
fn no_activation_record_means_direct() {
    let temp = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        active_executable(temp.path(), &accept),
        Err(Direct::NotActivated)
    );
}

#[test]
fn a_record_of_another_schema_is_not_an_activation() {
    let temp = activated_home();
    fs::write(activation_path(temp.path()), r#"{"schema": 2}"#).expect("record");
    assert_eq!(
        active_executable(temp.path(), &accept),
        Err(Direct::NotActivated)
    );
    fs::write(activation_path(temp.path()), "not json").expect("record");
    assert_eq!(
        active_executable(temp.path(), &accept),
        Err(Direct::NotActivated)
    );
}

#[test]
fn an_activated_verified_launcher_is_used() {
    let temp = activated_home();
    assert_eq!(
        active_executable(temp.path(), &accept),
        Ok(executable_path(temp.path()))
    );
}

#[test]
fn a_missing_or_symlinked_launcher_falls_back() {
    let temp = activated_home();
    let executable = executable_path(temp.path());
    fs::remove_file(&executable).expect("remove");
    assert_eq!(
        active_executable(temp.path(), &accept),
        Err(Direct::Missing)
    );
    let elsewhere = temp.path().join("elsewhere");
    fs::write(&elsewhere, "#!/bin/sh\n").expect("write");
    std::os::unix::fs::symlink(&elsewhere, &executable).expect("symlink");
    assert_eq!(
        active_executable(temp.path(), &accept),
        Err(Direct::Missing)
    );
}

#[test]
fn a_launcher_that_refuses_the_release_falls_back() {
    let temp = activated_home();
    let result = active_executable(temp.path(), &refuse);
    assert!(
        matches!(&result, Err(Direct::VerifyFailed(detail)) if detail.contains("exit 77")),
        "{result:?}"
    );
    assert!(result.unwrap_err().reason().starts_with("verify_failed: "));
}

#[test]
fn launcher_verify_reports_exit_status_and_stderr() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = temp.path().join("launcher");
    fs::write(&script, "#!/bin/sh\necho \"refused $1\" >&2\nexit 77\n").expect("write");
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    let error = launcher_verify(&script).expect_err("refused");
    assert_eq!(error, "exit 77: refused --launcher-verify");
    fs::write(&script, "#!/bin/sh\nexit 0\n").expect("write");
    assert_eq!(launcher_verify(&script), Ok(()));
}

#[test]
fn only_a_launchd_jobs_own_program_is_routed() {
    let exe = "/h/.local/share/shipyard/auth-generations/a/shipyard";
    assert!(launchd_job(exe).wants_routing());

    // A child of a job (the tick's shipyard calls) already inherits the job's
    // identity.
    let mut child = launchd_job(exe);
    child.ppid = 4242;
    assert!(!child.wants_routing());

    // An interactive shell is not a launchd job.
    for name in [None, Some(String::new()), Some("0".to_owned())] {
        let mut interactive = launchd_job(exe);
        interactive.xpc_service_name = name;
        assert!(!interactive.wants_routing());
    }

    let mut under = launchd_job(exe);
    under.under_launcher = true;
    assert!(!under.wants_routing(), "never route twice");

    let mut off = launchd_job(exe);
    off.routing_off = true;
    assert!(!off.wants_routing());
}

#[test]
fn routing_never_swaps_one_shipyard_for_another() {
    // A job pinned to some other build runs that build, not the release the
    // launcher would pick.
    assert!(!launchd_job("/h/.local/share/shipyard-steward/bin/shipyard").wants_routing());
    let mut unknown = launchd_job("/x");
    unknown.current_exe = None;
    unknown.pointer_target = None;
    assert!(!unknown.wants_routing());
}

#[test]
fn log_lines_land_in_the_launcher_log() {
    let temp = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(temp.path().join("Library/Logs")).expect("logs");
    log(temp.path(), "job=x launcher=direct reason=launcher_missing");
    let text = fs::read_to_string(log_path(temp.path())).expect("log");
    assert!(text.contains("shipyard job=x launcher=direct reason=launcher_missing"));
}

#[test]
fn the_daemon_itself_is_never_routed() {
    let args = |list: &[&str]| {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    };
    assert!(is_daemon_internal(args(&[
        "--mode", "shipyard", "daemon", "run", "--repo", "a/b"
    ])));
    assert!(is_daemon_internal(args(&[
        "daemon",
        "supervise",
        "--in-place",
        "--exec",
        "/x"
    ])));
    assert!(!is_daemon_internal(args(&[
        "daemon",
        "supervise",
        "--exec",
        "/x"
    ])));
    assert!(!is_daemon_internal(args(&[
        "--json",
        "runner",
        "fleet-reconcile",
        "--apply"
    ])));
    let mut daemon = launchd_job("/h/.local/share/shipyard/auth-generations/a/shipyard");
    daemon.daemon_internal = true;
    assert!(!daemon.wants_routing());
}
