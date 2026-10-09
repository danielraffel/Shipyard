//! `shipyard daemon supervise`: the launchd launcher's program. It must stay
//! resident as the daemon's parent and hand its stop signals to the daemon.

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_shipyard")
}

fn wait_for(path: &Path, what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(text) = fs::read_to_string(path)
            && !text.trim().is_empty()
        {
            return text;
        }
        assert!(Instant::now() < deadline, "{what} never appeared");
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn supervise_forwards_sigterm_to_the_daemon_and_exits_with_its_code() {
    let temp = tempfile::tempdir().expect("tempdir");
    let pid_file = temp.path().join("child.pid");
    let parent_file = temp.path().join("child.ppid");
    let marker = temp.path().join("terminated");
    let script = temp.path().join("fake-daemon");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ntrap 'echo term > \"{marker}\"; exit 7' TERM\necho $PPID > \"{ppid}\"\necho $$ > \"{pid}\"\nwhile :; do sleep 0.05; done\n",
            marker = marker.display(),
            ppid = parent_file.display(),
            pid = pid_file.display()
        ),
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut supervisor = Command::new(binary())
        .arg("--mode")
        .arg("shipyard")
        .arg("--global-dir")
        .arg(temp.path().join("global"))
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .args(["daemon", "supervise", "--exec"])
        .arg(&script)
        .args(["--repo", "owner/repo"])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn supervisor");

    let child_pid = wait_for(&pid_file, "daemon pid");
    assert_eq!(
        wait_for(&parent_file, "daemon parent pid").trim(),
        supervisor.id().to_string(),
        "the supervisor stays resident as the daemon's parent"
    );

    let status = Command::new("kill")
        .args(["-TERM", &supervisor.id().to_string()])
        .status()
        .expect("signal supervisor");
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(20);
    let exit = loop {
        if let Some(exit) = supervisor.try_wait().expect("try_wait") {
            break exit;
        }
        if Instant::now() >= deadline {
            let _ = Command::new("kill")
                .args(["-KILL", child_pid.trim()])
                .status();
            let _ = supervisor.kill();
            panic!("supervisor did not stop after SIGTERM");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        fs::read_to_string(&marker)
            .expect("daemon saw SIGTERM")
            .trim(),
        "term"
    );
    assert_eq!(
        exit.code(),
        Some(7),
        "the daemon's exit code is the supervisor's"
    );
}

#[test]
fn supervise_contract_is_printed_for_the_installer() {
    let output = Command::new(binary())
        .args(["daemon", "supervise", "--contract"])
        .output()
        .expect("run");
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "shipyard-daemon-supervise-v1"
    );
}

#[test]
fn in_place_step_prepares_the_daemon_and_execs_it_under_the_same_pid() {
    let temp = tempfile::tempdir().expect("tempdir");
    let state = temp.path().join("state");
    let report = temp.path().join("report");
    let script = temp.path().join("fake-daemon");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n{{ echo \"pid=$$\"; echo \"tmpdir=$TMPDIR\"; printf 'arg=%s\\n' \"$@\"; }} > \"{report}\"\necho from-daemon-stdout\n",
            report = report.display()
        ),
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");

    let mut child = Command::new(binary())
        .arg("--mode")
        .arg("shipyard")
        .arg("--global-dir")
        .arg(temp.path().join("global"))
        .arg("--state-dir")
        .arg(&state)
        .args(["daemon", "supervise", "--in-place", "--exec"])
        .arg(&script)
        .args(["--repo", "o/b", "--repo", "o/a"])
        .stdin(Stdio::null())
        .spawn()
        .expect("spawn in-place step");
    let status = child.wait().expect("wait");
    assert!(
        status.success(),
        "the exec'd daemon's exit status is the step's"
    );

    let report = fs::read_to_string(&report).expect("fake daemon ran");
    assert!(
        report.contains(&format!("pid={}\n", child.id())),
        "exec keeps the pid, so the launcher's privacy responsibility carries over: {report}"
    );
    assert!(
        report.contains(&format!("tmpdir={}\n", state.join("daemon/tmp").display())),
        "the release's own spawn code set the private TMPDIR: {report}"
    );
    let arguments: Vec<&str> = report
        .lines()
        .filter_map(|line| line.strip_prefix("arg="))
        .collect();
    let global = temp.path().join("global");
    assert_eq!(
        arguments,
        [
            "--mode",
            "shipyard",
            "--global-dir",
            &global.to_string_lossy(),
            "--state-dir",
            &state.to_string_lossy(),
            "daemon",
            "run",
            "--repo",
            "o/a",
            "--repo",
            "o/b",
        ],
        "the daemon argv is exactly a direct spawn's, which fleet-update evidence compares"
    );
    let log = fs::read_to_string(state.join("daemon/daemon.log")).expect("daemon log");
    assert!(
        log.contains("from-daemon-stdout"),
        "stdout goes to daemon.log"
    );
}

/// A fake daemon that records each start's pid, then runs until killed, or
/// exits 0 once `stop` exists.
fn write_counting_daemon(dir: &Path) -> std::path::PathBuf {
    let script = dir.join("fake-daemon");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ >> \"{starts}\"\nwhile [ ! -e \"{stop}\" ]; do sleep 0.05; done\nexit 0\n",
            starts = dir.join("starts").display(),
            stop = dir.join("stop").display(),
        ),
    )
    .expect("script");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn starts(dir: &Path) -> Vec<String> {
    fs::read_to_string(dir.join("starts"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn supervise_command(temp: &Path, script: &Path) -> Command {
    let mut command = Command::new(binary());
    command
        .arg("--mode")
        .arg("shipyard")
        .arg("--global-dir")
        .arg(temp.join("global"))
        .arg("--state-dir")
        .arg(temp.join("state"))
        .args(["daemon", "supervise", "--exec"])
        .arg(script)
        .args(["--repo", "owner/repo"])
        .stdin(Stdio::null());
    command
}

fn wait_exit(child: &mut std::process::Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(exit) = child.try_wait().expect("try_wait") {
            return exit;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("supervisor did not exit");
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// launchd's `KeepAlive {SuccessfulExit = false}` restarts the agent on a
/// non-zero exit, so `supervise` must report a killed daemon as non-zero and
/// a cleanly stopped one as zero.
#[test]
fn supervise_reports_a_killed_daemon_as_failure_and_a_stopped_one_as_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let script = write_counting_daemon(temp.path());

    let mut killed = supervise_command(temp.path(), &script)
        .spawn()
        .expect("spawn supervisor");
    let deadline = Instant::now() + Duration::from_secs(20);
    while starts(temp.path()).is_empty() {
        assert!(Instant::now() < deadline, "daemon never started");
        thread::sleep(Duration::from_millis(20));
    }
    let pid = starts(temp.path())[0].clone();
    assert!(
        Command::new("kill")
            .args(["-KILL", &pid])
            .status()
            .expect("kill")
            .success()
    );
    let exit = wait_exit(&mut killed);
    assert_eq!(
        exit.code(),
        Some(128 + 9),
        "SIGKILL reads as 137, a failure"
    );

    fs::write(temp.path().join("stop"), "").expect("stop marker");
    let mut stopped = supervise_command(temp.path(), &script)
        .spawn()
        .expect("spawn supervisor");
    assert_eq!(
        wait_exit(&mut stopped).code(),
        Some(0),
        "a clean stop is a success"
    );
}

/// The whole chain under the real launchd: a rendered daemon agent running
/// `shipyard daemon supervise`. Killing the daemon brings up a new pid; a
/// daemon that exits 0 stays down. Needs a logged-in GUI session (`gui/<uid>`)
/// and takes about two throttle intervals, so it runs on demand:
/// `cargo test --test daemon_supervise -- --ignored launchd`.
#[cfg(target_os = "macos")]
#[test]
#[ignore = "drives the real launchd gui domain; run on a macOS host with --ignored"]
fn launchd_restarts_a_killed_daemon_and_leaves_a_stopped_one_down() {
    use shipyard::daemon_launcher::{AgentKind, DAEMON_THROTTLE_SECS, render_plist};

    let temp = tempfile::tempdir().expect("tempdir");
    let script = write_counting_daemon(temp.path());
    let label = format!(
        "com.danielraffel.shipyard.test.keepalive.{}",
        std::process::id()
    );
    let plist = temp.path().join(format!("{label}.plist"));
    let arguments: Vec<std::ffi::OsString> = vec![
        binary().into(),
        "--mode".into(),
        "shipyard".into(),
        "--global-dir".into(),
        temp.path().join("global").into(),
        "--state-dir".into(),
        temp.path().join("state").into(),
        "daemon".into(),
        "supervise".into(),
        "--exec".into(),
        script.clone().into(),
        "--repo".into(),
        "owner/repo".into(),
    ];
    fs::write(
        &plist,
        render_plist(
            &label,
            &arguments,
            &temp.path().join("launcher.log"),
            temp.path(),
            std::ffi::OsStr::new("/usr/bin:/bin"),
            AgentKind::Daemon,
        ),
    )
    .expect("plist");
    let uid =
        String::from_utf8(Command::new("id").arg("-u").output().expect("id").stdout).expect("utf8");
    let domain = format!("gui/{}", uid.trim());
    let target = format!("{domain}/{label}");
    struct Bootout(String);
    impl Drop for Bootout {
        fn drop(&mut self) {
            let _ = Command::new("/bin/launchctl")
                .args(["bootout", &self.0])
                .status();
        }
    }
    let _cleanup = Bootout(target.clone());
    let status = Command::new("/bin/launchctl")
        .args(["bootstrap", &domain])
        .arg(&plist)
        .status()
        .expect("bootstrap");
    assert!(status.success(), "launchctl bootstrap {domain} failed");
    // RunAtLoad starts it; kickstart makes that start immediate, as
    // `start_via_launchd` does.
    let _ = Command::new("/bin/launchctl")
        .args(["kickstart", &target])
        .status();

    let wait_for_starts = |count: usize, within: Duration| {
        let deadline = Instant::now() + within;
        while starts(temp.path()).len() < count {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
        true
    };
    let throttle = Duration::from_secs(u64::from(DAEMON_THROTTLE_SECS));
    assert!(
        wait_for_starts(1, Duration::from_secs(30)),
        "launchd never started the agent"
    );
    let first = starts(temp.path())[0].clone();
    assert!(
        Command::new("kill")
            .args(["-KILL", &first])
            .status()
            .expect("kill")
            .success()
    );
    assert!(
        wait_for_starts(2, throttle * 2 + Duration::from_secs(15)),
        "launchd did not restart the agent after the daemon was killed"
    );
    let second = starts(temp.path())[1].clone();
    assert_ne!(first, second, "a new daemon pid after the kill");

    // Negative control: a clean exit is not restarted.
    fs::write(temp.path().join("stop"), "").expect("stop marker");
    assert!(
        !wait_for_starts(3, throttle * 2 + Duration::from_secs(15)),
        "launchd restarted a daemon that exited 0: {:?}",
        starts(temp.path())
    );
}
