//! Production-shaped installed auth-generation acceptance controls.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

const GHAPP: &[u8] = include_bytes!("../scripts/ghapp");

fn write_private(path: &Path, contents: &[u8], mode: u32) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("fixture mode");
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the acceptance control intentionally keeps one ordered installed-path scenario"
)]
fn installed_generation_uses_real_sibling_shipyard_and_machine_global_auth_under_env_i() {
    let temp = tempfile::tempdir().expect("fixture home");
    let home = temp.path();
    let bin = home.join(".local/bin");
    let generation_id = "7".repeat(64);
    let authority_id = "8".repeat(64);
    let generation_parent = home.join(".local/share/shipyard");
    let generation_store = generation_parent.join("auth-generations");
    let generation = generation_store.join(&generation_id);
    let global_dir = home.join("Library/Application Support/shipyard");
    fs::create_dir_all(&bin).expect("bin");
    fs::create_dir_all(&generation).expect("generation");
    fs::create_dir_all(&global_dir).expect("global dir");
    for private_dir in [&generation_parent, &generation_store, &generation] {
        fs::set_permissions(private_dir, fs::Permissions::from_mode(0o700))
            .expect("private generation directory");
    }

    let public_wrapper = bin.join("ghapp");
    let generation_wrapper = generation.join("ghapp");
    let generation_binary = generation.join("shipyard");
    let generation_helper = generation.join("shipyard-github-app-token");
    let generation_context = generation.join("ghapp.shipyard-context.json");
    let private_key = home.join("private-key.pem");
    write_private(&generation_wrapper, GHAPP, 0o700);
    fs::copy(env!("CARGO_BIN_EXE_shipyard"), &generation_binary).expect("real sibling binary");
    fs::set_permissions(&generation_binary, fs::Permissions::from_mode(0o700))
        .expect("binary mode");
    write_private(
        &generation_helper,
        b"#!/usr/bin/python3\nimport json\nprint(json.dumps({\"token\": \"ghs_generation_fixture\"}))\n",
        0o700,
    );
    write_private(&private_key, b"offline-test-key\n", 0o600);
    write_private(
        &generation_context,
        serde_json::to_string(&serde_json::json!({
            "schema_version": 2,
            "mode": "shipyard",
            "global_dir": global_dir,
            "authority_identity": authority_id,
            "generation_id": generation_id,
        }))
        .expect("context json")
        .as_bytes(),
        0o600,
    );
    symlink(&generation_wrapper, &public_wrapper).expect("atomic selector fixture");

    write_private(
        &global_dir.join("config.toml"),
        format!(
            "[github.auth]\nsource = \"command\"\ntoken_command = [{:?}, \"token\", \"--app-id\", \"123456\", \"--private-key\", {:?}, \"--repo\", \"{{repo_slug}}\"]\n",
            public_wrapper.display().to_string(),
            private_key.display().to_string(),
        )
        .as_bytes(),
        0o600,
    );
    let checkout = home.join("checkout");
    fs::create_dir(&checkout).expect("checkout");
    assert!(
        Command::new("/usr/bin/git")
            .args(["init", "--quiet"])
            .current_dir(&checkout)
            .status()
            .expect("git init")
            .success()
    );
    assert!(
        Command::new("/usr/bin/git")
            .args([
                "remote",
                "add",
                "origin",
                "https://github.com/danielraffel/Shipyard.git",
            ])
            .current_dir(&checkout)
            .status()
            .expect("git remote")
            .success()
    );

    let native_gh = [
        PathBuf::from("/opt/homebrew/bin/gh"),
        PathBuf::from("/usr/local/bin/gh"),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
    .expect("release host must provide native gh at the wrapper's trusted path");
    let output = Command::new(&public_wrapper)
        .args(["auth", "token"])
        .current_dir(&checkout)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()
        .expect("env-i installed wrapper");
    assert!(
        output.status.success(),
        "native gh={} status={:?} stderr={}",
        native_gh.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("UTF-8 token")
            .trim(),
        "ghs_generation_fixture"
    );
}
