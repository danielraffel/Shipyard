//! Real process environment coverage without mutating the test runner's environment.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

fn shipyard(root: &Path, args: &[&str]) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_shipyard"))
        .current_dir(root)
        .args(["--mode", "isolated", "--global-dir"])
        .arg(root.join("global"))
        .arg("--state-dir")
        .arg(root.join("state"))
        .arg("--json")
        .args(args)
        .env("SHIPYARD_OBSERVATIONS", "  OfF\t")
        .output()
        .expect("run isolated CLI");
    assert!(
        output.status.success(),
        "{args:?}: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

#[test]
fn optional_observations_off_preserves_evidence_explicit_imports_and_history() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir(root.join("global")).unwrap();
    std::fs::write(
        root.join("global/config.toml"),
        "[targets.local]\nbackend = \"local\"\nplatform = \"macos-arm64\"\n\
         [validation.default]\ncommand = \"true\"\n",
    )
    .unwrap();
    let command = shipyard(
        root,
        &["run", "command", "--target", "local", "--", "echo", "proof"],
    );
    assert_eq!(command["evidence"]["status"], "pass");
    assert!(
        Path::new(command["evidence"]["bundle_path"].as_str().unwrap())
            .join("evidence.json")
            .is_file()
    );
    assert!(Path::new(command["evidence"]["log_path"].as_str().unwrap()).is_file());
    assert!(
        !root.join("state/metrics").exists(),
        "automatic metric copy is off"
    );

    shipyard(
        root,
        &[
            "metrics",
            "record",
            "--project",
            "pulp",
            "--repo",
            "o/pulp",
            "--job",
            "explicit",
            "--duration-ms",
            "12",
        ],
    );
    let rows = shipyard(root, &["metrics", "list"]);
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
    assert_eq!(rows["rows"][0]["job"], "explicit");

    let receipt = json!({
        "schema": "shipyard-merge-receipt/v1", "receipt_id": "keep-authority",
        "repo": "o/pulp", "project": "pulp", "base_branch": "main", "pr": 42,
        "head_sha": "a".repeat(40), "validated_head_sha": "a".repeat(40),
        "merge_commit_sha": "b".repeat(40), "merged_at": "2026-10-30T00:00:00Z",
        "safe_merge": true, "source": "shipyard", "required_checks": ["gate"],
        "gate_receipt_sha256": "c".repeat(64), "evidence": "sha256:receipt"
    });
    std::fs::write(root.join("receipt.jsonl"), format!("{receipt}\n")).unwrap();
    let imported = shipyard(
        root,
        &[
            "metrics",
            "import",
            "merge-receipt",
            "--file",
            "receipt.jsonl",
        ],
    );
    assert_eq!(imported["imported"], 1);
    shipyard(
        root,
        &[
            "metrics",
            "export",
            "merge-receipt",
            "--repo",
            "o/pulp",
            "--file",
            "export.jsonl",
        ],
    );
    let exported: Value =
        serde_json::from_str(&std::fs::read_to_string(root.join("export.jsonl")).unwrap()).unwrap();
    assert_eq!(exported, receipt);
    let rows = shipyard(root, &["metrics", "list"]);
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
}
