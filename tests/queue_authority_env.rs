//! A Shipyard process never passes queue authority on to what it spawns.
//!
//! The internal marker, the review-only switch, the nonce variables and every
//! `GHAPP_ALLOW_*` override are dropped before the process does anything, so
//! a runner, validation command or hook it starts cannot carry them.

#![cfg(unix)]

use std::process::Command;

use serde_json::Value;

const AUTHORITY: [(&str, &str); 6] = [
    ("SHIPYARD_INTERNAL_QUEUE_MUTATION", "1"),
    ("SHIPYARD_QUEUE_ARM_REVIEW_ONLY", "1"),
    ("SHIPYARD_QUEUE_MUTATION_NONCE", "00"),
    ("SHIPYARD_QUEUE_MUTATION_NONCE_DIR", "/tmp"),
    ("GHAPP_ALLOW_UNREVIEWED_ARM", "1"),
    ("GHAPP_ALLOW_QUEUE_REARM", "1"),
];

fn shipyard(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shipyard"));
    command
        .env("HOME", home)
        .env_remove("XDG_STATE_HOME")
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME");
    command
}

#[test]
fn a_child_shipyard_spawns_inherits_no_queue_authority() {
    let temp = tempfile::tempdir().expect("tempdir");
    let paths = shipyard(temp.path())
        .args(["paths", "--json"])
        .output()
        .expect("paths");
    assert!(paths.status.success(), "{paths:?}");
    let paths: Value = serde_json::from_slice(&paths.stdout).expect("paths json");
    let state_dir = paths["state_dir"].as_str().expect("state_dir");
    std::fs::create_dir_all(state_dir).expect("state dir");

    let output = shipyard(temp.path())
        .envs(AUTHORITY)
        .env("SHIPYARD_UNRELATED_PROBE", "kept")
        .args(["writer-domain-exec", "--path"])
        .arg(std::path::Path::new(state_dir).join("probe"))
        .args(["--", "/usr/bin/env"])
        .output()
        .expect("writer-domain-exec");
    assert!(output.status.success(), "{output:?}");
    let env = String::from_utf8_lossy(&output.stdout);
    assert!(env.contains("SHIPYARD_UNRELATED_PROBE=kept"), "{env}");
    for (name, _) in AUTHORITY {
        assert!(
            !env.lines()
                .any(|line| line.starts_with(&format!("{name}="))),
            "{name} reached the child:\n{env}"
        );
    }
}
