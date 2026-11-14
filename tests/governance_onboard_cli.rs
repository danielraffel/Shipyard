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
    let mut gh_binary = vec![
        "--gh-binary".to_owned(),
        bin.join("ghapp").display().to_string(),
    ];
    gh_binary.extend(extra.iter().map(|arg| (*arg).to_owned()));
    onboard_with_args(root, &gh_binary)
}

fn onboard_with_args(root: &Path, extra: &[String]) -> std::process::Output {
    let bin = root.join("bin");
    Command::new(env!("CARGO_BIN_EXE_shipyard"))
        .current_dir(root)
        .args(["--mode", "isolated", "--global-dir"])
        .arg(root.join("global"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .args(["governance", "onboard", "--repo", "danielraffel/spectr"])
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

#[test]
fn an_unreadable_config_fails_instead_of_falling_back_to_ambient_gh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join("global")).unwrap();
    std::fs::create_dir_all(root.join(".shipyard")).unwrap();
    std::fs::write(
        root.join(".shipyard/config.toml"),
        "this is = = not toml [\n",
    )
    .unwrap();
    let log = root.join("gh.log");
    for name in ["gh", "ghapp", "ghread"] {
        logging_gh(&bin, name, &log);
    }

    // No --gh-binary: the identity must come from the config, and a config
    // that cannot be read must stop the plan before any CLI is started.
    let output = onboard_with_args(root, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("config"),
        "{output:?}"
    );
    assert!(
        !log.exists(),
        "a gh-compatible CLI was started: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // Control: an explicit --gh-binary does not need the config.
    let output = onboard(root, &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "the fake answers nothing usable"
    );
    assert!(log.exists(), "control: --gh-binary reaches the fake CLI");
}

#[test]
fn an_unreadable_local_overlay_fails_instead_of_falling_back_to_ambient_gh() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(root.join(".shipyard-dev.local")).unwrap();
    std::fs::write(
        root.join(".shipyard-dev.local/config.toml"),
        "this is = = not toml [\n",
    )
    .unwrap();
    let log = root.join("gh.log");
    for name in ["gh", "ghapp", "ghread"] {
        logging_gh(&bin, name, &log);
    }

    // The malformed local overlay must stop the plan before any CLI runs.
    let output = onboard_with_args(root, &[]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("config"),
        "{output:?}"
    );
    assert!(
        !log.exists(),
        "a gh-compatible CLI was started: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );

    // Control: an explicit --gh-binary bypasses config loading by design.
    let output = onboard(root, &[]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "the fake answers nothing usable"
    );
    assert!(log.exists(), "control: --gh-binary reaches the fake CLI");
}
