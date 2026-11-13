//! `shipyard governance onboard --apply` is not implemented: it must refuse
//! before any GitHub read, so no gh-compatible CLI is ever started.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

fn logging_gh(dir: &Path, name: &str, log: &Path) {
    let path = dir.join(name);
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\necho \"{name} $*\" >> '{}'\necho '{{}}'\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn onboard(root: &Path, extra: &[&str]) -> std::process::Output {
    let bin = root.join("bin");
    Command::new(env!("CARGO_BIN_EXE_shipyard"))
        .current_dir(root)
        .args(["--mode", "isolated", "--global-dir"])
        .arg(root.join("global"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .args(["governance", "onboard", "--repo", "danielraffel/spectr"])
        .arg("--gh-binary")
        .arg(bin.join("ghapp"))
        .args(extra)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("HOME", root)
        .output()
        .expect("run isolated CLI")
}

#[test]
fn apply_exits_1_without_starting_any_gh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join("global")).unwrap();
    let log = root.join("gh.log");
    for name in ["gh", "ghapp", "ghread"] {
        logging_gh(&bin, name, &log);
    }

    let output = onboard(root, &["--apply"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--apply is not implemented"),
        "{output:?}"
    );
    assert!(
        !log.exists(),
        "gh was started: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // Control: without --apply the same setup does reach the fake CLI.
    let output = onboard(root, &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "the fake answers nothing usable"
    );
    assert!(log.exists(), "control: the plan reads through the fake CLI");
}
