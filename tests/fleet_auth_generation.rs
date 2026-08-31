//! Production-shaped installed auth-generation acceptance controls.
#![cfg(target_os = "macos")]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const GHAPP: &[u8] = include_bytes!("../scripts/ghapp");
const PR_CLOSE_GUARD: &[u8] = include_bytes!("../scripts/ghapp_pr_close_guard.py");
const PUBLIC_TRAMPOLINE_END: &[u8] = b"# Shipyard-Stable-Public-Trampoline-END\n";

fn public_trampoline() -> Vec<u8> {
    let marker_offset = GHAPP
        .windows(PUBLIC_TRAMPOLINE_END.len())
        .position(|window| window == PUBLIC_TRAMPOLINE_END)
        .expect("public trampoline end marker");
    let mut trampoline = GHAPP[..marker_offset + PUBLIC_TRAMPOLINE_END.len()].to_vec();
    trampoline
        .extend_from_slice(b"echo \"ghapp: stable public trampoline fell through\" >&2\nexit 1\n");
    trampoline
}

fn write_private(path: &Path, contents: &[u8], mode: u32) {
    fs::write(path, contents).expect("write fixture");
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("fixture mode");
}

fn sha256_file(path: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(fs::read(path).expect("fixture member bytes"))
    )
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
    let generation_public_trampoline = generation.join("ghapp.public-trampoline");
    let generation_binary = generation.join("shipyard");
    let generation_helper = generation.join("shipyard-github-app-token");
    let generation_context = generation.join("ghapp.shipyard-context.json");
    let private_key = home.join("private-key.pem");
    let public_trampoline = public_trampoline();
    assert_eq!(
        format!("{:x}", Sha256::digest(&public_trampoline)),
        "ca21046ccb436a989c1316665fcc0d13d36828fd67c76848896130127c8c030a",
        "the successor must preserve the exact v0.137 public trampoline bytes",
    );
    write_private(&generation_wrapper, GHAPP, 0o700);
    write_private(&generation_public_trampoline, &public_trampoline, 0o700);
    write_private(&generation.join("pr-close-guard"), PR_CLOSE_GUARD, 0o700);
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
    write_private(
        &generation.join("generation.manifest"),
        format!(
            "schema_version=1\ngeneration_contract=auth-selector-v2\ngeneration_id={generation_id}\nauthority_identity={authority_id}\nhelper_sha256={}\nhelper_mode=700\nwrapper_sha256={}\nwrapper_mode=700\npublic_trampoline_sha256={}\npublic_trampoline_mode=700\nclose_guard_sha256={}\nclose_guard_mode=700\nbinary_sha256={}\nbinary_mode=700\ncompanion_sha256=absent\ncontext_sha256={}\ncontext_template_sha256={}\n",
            sha256_file(&generation_helper),
            sha256_file(&generation_wrapper),
            sha256_file(&generation_public_trampoline),
            sha256_file(&generation.join("pr-close-guard")),
            sha256_file(&generation_binary),
            sha256_file(&generation_context),
            "6".repeat(64),
        )
        .as_bytes(),
        0o600,
    );
    write_private(&public_wrapper, &public_trampoline, 0o700);
    symlink(
        &generation_wrapper,
        public_wrapper.with_extension("shipyard-generation"),
    )
    .expect("atomic selector fixture");

    write_private(
        &global_dir.join("config.toml"),
        format!(
            "[github.auth]\nsource = \"command\"\ntoken_command = [{:?}, \"token\", \"--app-id\", \"123456\", \"--private-key\", {:?}, \"--repo\", \"{{repo_slug}}\"]\n",
            generation_wrapper.display().to_string(),
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
    let remediated = fs::read_to_string(global_dir.join("config.toml"))
        .expect("remediated machine-global config");
    assert!(remediated.contains(&public_wrapper.display().to_string()));
    assert!(!remediated.contains(&generation_wrapper.display().to_string()));
    assert_eq!(
        fs::metadata(global_dir.join("config.toml"))
            .expect("remediated machine-global config metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "atomic remediation must preserve private machine-global config permissions",
    );

    let close_guard = generation.join("pr-close-guard");
    let close_guard_bytes = fs::read(&close_guard).expect("close guard bytes");
    write_private(&close_guard, b"tampered-close-guard\n", 0o700);
    write_private(
        &global_dir.join("config.toml"),
        format!(
            "[github.auth]\nsource = \"command\"\ntoken_command = [{:?}, \"token\", \"--app-id\", \"123456\", \"--private-key\", {:?}, \"--repo\", \"{{repo_slug}}\"]\n",
            generation_wrapper.display().to_string(),
            private_key.display().to_string(),
        )
        .as_bytes(),
        0o600,
    );
    let rejected = Command::new(&public_wrapper)
        .args(["auth", "token"])
        .current_dir(&checkout)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()
        .expect("tampered manifest member");
    assert!(!rejected.status.success());
    assert!(
        fs::read_to_string(global_dir.join("config.toml"))
            .expect("preserved tampered-member config")
            .contains(&generation_wrapper.display().to_string()),
        "a manifest member mismatch must refuse without mutation",
    );
    write_private(&close_guard, &close_guard_bytes, 0o700);

    let spoofed_generation = generation_store.join("f".repeat(64));
    fs::create_dir(&spoofed_generation).expect("spoofed generation directory");
    fs::set_permissions(&spoofed_generation, fs::Permissions::from_mode(0o700))
        .expect("spoofed generation mode");
    let spoofed_wrapper = spoofed_generation.join("ghapp");
    for (name, mode) in [
        ("ghapp", 0o700),
        ("ghapp.public-trampoline", 0o700),
        ("shipyard-github-app-token", 0o700),
        ("pr-close-guard", 0o700),
        ("shipyard", 0o700),
        ("ghapp.shipyard-context.json", 0o600),
        ("generation.manifest", 0o600),
    ] {
        fs::copy(generation.join(name), spoofed_generation.join(name))
            .expect("copy alternate generation member");
        fs::set_permissions(
            spoofed_generation.join(name),
            fs::Permissions::from_mode(mode),
        )
        .expect("alternate generation member mode");
    }
    fs::remove_file(public_wrapper.with_extension("shipyard-generation"))
        .expect("remove original selector");
    symlink(
        &spoofed_wrapper,
        public_wrapper.with_extension("shipyard-generation"),
    )
    .expect("install mismatched selector");
    write_private(
        &global_dir.join("config.toml"),
        format!(
            "[github.auth]\nsource = \"command\"\ntoken_command = [{:?}, \"token\", \"--app-id\", \"123456\", \"--private-key\", {:?}, \"--repo\", \"{{repo_slug}}\"]\n",
            generation_wrapper.display().to_string(),
            private_key.display().to_string(),
        )
        .as_bytes(),
        0o600,
    );
    let rejected = Command::new(&public_wrapper)
        .args(["auth", "token"])
        .current_dir(&checkout)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()
        .expect("selector/config generation mismatch");
    assert!(!rejected.status.success());
    assert!(!String::from_utf8_lossy(&rejected.stdout).contains("ghs_generation_fixture"));
    assert!(
        fs::read_to_string(global_dir.join("config.toml"))
            .expect("preserved mismatched config")
            .contains(&generation_wrapper.display().to_string()),
        "a selector/config generation mismatch must refuse without mutation",
    );

    fs::remove_file(public_wrapper.with_extension("shipyard-generation"))
        .expect("remove mismatched selector");
    symlink(
        &generation_wrapper,
        public_wrapper.with_extension("shipyard-generation"),
    )
    .expect("restore original selector");
    fs::remove_file(spoofed_generation.join("generation.manifest"))
        .expect("remove spoofed manifest");
    write_private(
        &global_dir.join("config.toml"),
        format!(
            "[github.auth]\nsource = \"command\"\ntoken_command = [{:?}, \"token\", \"--app-id\", \"123456\", \"--private-key\", {:?}, \"--repo\", \"{{repo_slug}}\"]\n",
            spoofed_wrapper.display().to_string(),
            private_key.display().to_string(),
        )
        .as_bytes(),
        0o600,
    );
    let rejected = Command::new(&public_wrapper)
        .args(["auth", "token"])
        .current_dir(&checkout)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .output()
        .expect("unmanifested generation config");
    assert!(!rejected.status.success());
    assert!(
        fs::read_to_string(global_dir.join("config.toml"))
            .expect("preserved unmanifested config")
            .contains(&spoofed_wrapper.display().to_string()),
        "an unmanifested generation alias must not be remediated",
    );
}
