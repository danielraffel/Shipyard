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
fn the_record_file_alone_never_activates() {
    // Whatever the file says, only the launcher's verdict routes.
    let temp = activated_home();
    for body in [r#"{"schema": 2, "cdhash": "x"}"#, "not json", ""] {
        fs::write(activation_path(temp.path()), body).expect("record");
        let result = active_executable(temp.path(), &refuse);
        assert!(matches!(result, Err(Direct::VerifyFailed(_))), "{result:?}");
    }
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

fn fake_launcher(dir: &Path, body: &str) -> PathBuf {
    let script = dir.join("launcher");
    fs::write(
        &script,
        format!("#!/bin/sh\n[ \"$1\" = --launcher-active ] || exit 64\n{body}\n"),
    )
    .expect("write");
    fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod");
    script
}

#[test]
fn launcher_active_needs_exit_zero_and_active_true() {
    let temp = tempfile::tempdir().expect("tempdir");
    let active = fake_launcher(
        temp.path(),
        r#"echo '{"active":true,"reason":"grant_cached"}'"#,
    );
    assert_eq!(launcher_active(&active), Ok(()));

    let cases = [
        (
            r#"echo '{"active":false,"reason":"grant_revoked: demoted"}'; exit 1"#,
            "exit 1: grant_revoked: demoted",
        ),
        // Exit 0 without an explicit verdict is not active.
        (
            r#"echo '{"reason":"grant_cached"}'"#,
            "exit 0: grant_cached",
        ),
        (r#"echo '{"active":"yes"}'"#, "exit 0: "),
        ("echo garbage", "exit 0: "),
        // A refusal before the verdict (exit 77) carries its stderr.
        (
            "echo 'shipyard-launcher: not a Developer ID signed Shipyard' >&2; exit 77",
            "exit 77: shipyard-launcher: not a Developer ID signed Shipyard",
        ),
    ];
    for (body, expected) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let script = fake_launcher(dir.path(), body);
        assert_eq!(launcher_active(&script), Err(expected.to_owned()), "{body}");
    }
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
    assert!(is_daemon_internal(args(&[
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

#[test]
fn a_hung_launcher_is_not_active() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = fake_launcher(temp.path(), "sleep 30");
    let started = std::time::Instant::now();
    let error = launcher_active_within(&script, Duration::from_secs(1)).expect_err("timeout");
    assert_eq!(error, "timed out after 1s");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn an_exec_failure_is_logged_and_shipyard_runs_directly() {
    let temp = activated_home();
    fs::create_dir_all(temp.path().join("Library/Logs")).expect("logs");
    let facts = launchd_job("/h/.local/share/shipyard/auth-generations/a/shipyard");
    let execs = std::cell::RefCell::new(Vec::new());
    let exec = |path: &Path| {
        execs.borrow_mut().push(path.to_path_buf());
        std::io::Error::from(std::io::ErrorKind::PermissionDenied)
    };
    let line = route_with(&facts, temp.path(), &accept, &exec).expect("logged");
    assert_eq!(*execs.borrow(), [executable_path(temp.path())]);
    assert!(
        line.starts_with(
            "job=com.danielraffel.shipyard.fleet-reconcile launcher=direct reason=exec_failed"
        ),
        "{line}"
    );
    let log = fs::read_to_string(log_path(temp.path())).expect("log");
    assert!(log.contains("launcher=app") && log.contains("reason=exec_failed"));
}

#[test]
fn not_active_or_missing_never_execs() {
    let exec = |_: &Path| -> std::io::Error { panic!("must not exec") };
    let facts = launchd_job("/h/.local/share/shipyard/auth-generations/a/shipyard");

    let none = tempfile::tempdir().expect("tempdir");
    assert_eq!(route_with(&facts, none.path(), &accept, &exec), None);

    let refused = activated_home();
    let line = route_with(&facts, refused.path(), &refuse, &exec).expect("logged");
    assert!(line.contains("reason=verify_failed: exit 77"), "{line}");

    let missing = activated_home();
    fs::remove_file(executable_path(missing.path())).expect("remove");
    let line = route_with(&facts, missing.path(), &accept, &exec).expect("logged");
    assert!(line.ends_with("reason=launcher_missing"), "{line}");

    let mut daemon = facts.clone();
    daemon.daemon_internal = true;
    assert_eq!(route_with(&daemon, refused.path(), &accept, &exec), None);
}

#[test]
fn a_launcher_fallback_is_never_routed_again() {
    assert!(is_marked(|name| name == "SHIPYARD_LAUNCHER_FALLBACK"));
    assert!(is_marked(|name| name == "SHIPYARD_LAUNCHER_APP"));
    assert!(!is_marked(|_| false));
}
