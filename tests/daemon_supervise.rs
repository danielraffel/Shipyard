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
    assert!(
        temp.path().join("state/daemon/daemon.log").is_file(),
        "the daemon writes the same log a direct spawn does"
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
