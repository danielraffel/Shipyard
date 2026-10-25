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

/// `shipyard queue-hold exec -- <cmd>` with the marker and a valid nonce
/// exported: the held command sees neither, and a guard call it makes is
/// judged by every rule. Without the strip the guard would honour the
/// exported marker and nonce and step aside.
#[cfg(target_os = "macos")]
#[test]
fn a_queue_hold_child_inherits_no_queue_authority_and_is_judged_in_full() {
    let temp = tempfile::tempdir().expect("tempdir");
    let nonces = temp.path().join("nonces");
    let nonce = shipyard::queue_arm_gate::issue_nonce(
        &nonces,
        &[
            shipyard::queue_arm_gate::NONCE_INTERNAL_ARM,
            shipyard::queue_arm_gate::NONCE_REVIEW_ONLY,
        ],
    )
    .expect("nonce");
    let guard = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join("ghapp_queue_arm_guard.py");
    let env_dump = temp.path().join("child-env");
    let verdict = temp.path().join("guard");
    // The held command dumps its environment, then asks the guard to land a
    // request no head can be read from.
    let script = "env > \"$1\"; python3 \"$2\" api graphql --input - </dev/null 2> \"$3\"; echo $? >> \"$3\"";
    let output = shipyard(temp.path())
        .envs(AUTHORITY)
        .env("SHIPYARD_INTERNAL_QUEUE_MUTATION", "1")
        .env("SHIPYARD_QUEUE_MUTATION_NONCE", &nonce)
        .env("SHIPYARD_QUEUE_MUTATION_NONCE_DIR", &nonces)
        .env("GH_REPO", "owner/repo")
        .arg("--state-dir")
        .arg(temp.path().join("state"))
        .args([
            "queue-hold",
            "exec",
            "--purpose",
            "tartci-pool-off",
            "--host-id",
            "test-m1",
            "--service",
            "actions.runner.test.service",
            "--repo",
            "owner/repo",
            "--runner",
            "runner-one",
            "--",
            "/bin/sh",
            "-c",
            script,
            "held",
        ])
        .arg(&env_dump)
        .arg(&guard)
        .arg(&verdict)
        .output()
        .expect("queue-hold");
    assert!(output.status.success(), "{output:?}");
    let env = std::fs::read_to_string(&env_dump).expect("child env");
    for (name, _) in AUTHORITY {
        assert!(
            !env.lines()
                .any(|line| line.starts_with(&format!("{name}="))),
            "{name} reached the held command:\n{env}"
        );
    }
    let judged = std::fs::read_to_string(&verdict).expect("guard output");
    assert!(
        judged.contains("refusing ambiguous queue-arm request"),
        "{judged}"
    );
    assert_eq!(judged.lines().last(), Some("1"), "{judged}");
}
