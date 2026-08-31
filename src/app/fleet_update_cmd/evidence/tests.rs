#[cfg(unix)]
use std::process::Stdio;

use super::*;
use crate::capacity::HostClassConfig;

#[test]
fn local_refresh_pid_accepts_current_and_legacy_typed_receipts_only() {
    assert_eq!(
        local_refresh_daemon_pid(&serde_json::json!({
            "command": "daemon:refresh",
            "new_pid": 42
        })),
        Some(42)
    );
    assert_eq!(
        local_refresh_daemon_pid(
            &serde_json::json!({"event": "daemon_refreshed", "daemon_pid": 41})
        ),
        Some(41)
    );
    assert_eq!(
        local_refresh_daemon_pid(&serde_json::json!({"command": "other", "new_pid": 42})),
        None
    );
}

#[test]
fn local_refresh_pid_survives_preceding_update_event_stream() {
    let stream = br#"{"command":"update","event":"apply"}
{"command":"update","event":"applied"}
{"schema_version":1,"command":"daemon:refresh","new_pid":4242}
"#;
    assert_eq!(local_refresh_daemon_pid_from_output(stream), Some(4242));
    assert_eq!(
        local_refresh_daemon_pid_from_output(
            br#"{"command":"update","event":"apply"}
{"command":"update","event":"applied"}
"#,
        ),
        None
    );
}

#[test]
fn local_auth_launch_probe_requires_the_exact_typed_credential_contract() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let valid = serde_json::json!({
        "schema_version": 1,
        "command": "auth.helper-argv",
        "wrapper": plan.auth_wrapper,
        "repo": "danielraffel/Shipyard",
        "credential_argv": [
            "--app-id",
            "123456",
            "--private-key",
            "/Users/ci/.config/shipyard/github-app.pem",
        ],
    });
    validate_auth_launch_probe(&plan, valid.to_string().as_bytes()).expect("valid probe");

    let mut invalid = Vec::new();
    let mut value = valid.clone();
    value["schema_version"] = serde_json::json!(2);
    invalid.push(value);
    let mut value = valid.clone();
    value["extra"] = serde_json::json!(true);
    invalid.push(value);
    for argv in [
        serde_json::json!(["--wrong", "123", "--private-key", "/safe/key.pem"]),
        serde_json::json!(["--app-id", "0", "--private-key", "/safe/key.pem"]),
        serde_json::json!([
            "--app-id",
            "123456789012345678901",
            "--private-key",
            "/safe/key.pem"
        ]),
        serde_json::json!(["--app-id", "123", "--private-key", "relative/key.pem"]),
        serde_json::json!(["--app-id", "123", "--private-key", "/safe/../key.pem"]),
    ] {
        let mut value = valid.clone();
        value["credential_argv"] = argv;
        invalid.push(value);
    }
    for value in invalid {
        assert!(
            validate_auth_launch_probe(&plan, value.to_string().as_bytes()).is_err(),
            "accepted invalid auth probe {value}"
        );
    }
}

#[test]
fn local_generation_context_binds_mode_and_global_directory() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("ghapp.shipyard-context.json");
    let generation_id = "7".repeat(64);
    let authority_identity = "8".repeat(64);
    let valid = serde_json::json!({
        "schema_version": 2,
        "mode": plan.runtime_mode.as_str(),
        "global_dir": plan.global_dir,
        "generation_id": generation_id,
        "authority_identity": authority_identity,
    });
    let member = GenerationMemberEvidence {
        path: path.clone(),
        sha256: "d".repeat(64),
        mode: 0o600,
    };
    std::fs::write(&path, valid.to_string()).expect("context");
    validate_generation_context(&member, &generation_id, &authority_identity, &plan)
        .expect("valid context");

    for (field, replacement) in [
        ("mode", serde_json::json!("direct")),
        ("global_dir", serde_json::json!("/different/governed/root")),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = replacement;
        std::fs::write(&path, invalid.to_string()).expect("invalid context");
        assert!(
            validate_generation_context(&member, &generation_id, &authority_identity, &plan)
                .is_err(),
            "accepted context with wrong {field}"
        );
    }
}

fn verified_source_identity() -> String {
    "8".repeat(64)
}

fn host(ssh: Option<&str>) -> HostClassConfig {
    HostClassConfig {
        class: "m5".to_owned(),
        ssh: ssh.map(str::to_owned),
        cap: 2,
        tart_bin: "/opt/homebrew/bin/tart".to_owned(),
        tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
        shipyard_bin: Some("/Users/ci/.local/bin/shipyard".to_owned()),
        shipyard_mode: Some("shipyard".to_owned()),
        shipyard_global_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
        shipyard_state_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
        github_cli: Some("/Users/ci/.local/bin/ghapp".to_owned()),
        github_token_helper: Some(
            "/Users/ci/.config/shipyard/bin/shipyard-github-app-token".to_owned(),
        ),
        tart_home: Some("/Users/ci/VMs".to_owned()),
        labels: Vec::new(),
    }
}

fn pair(version: &str, verified: bool) -> BinaryPairEvidence {
    let source_identity = verified.then(verified_source_identity);
    let source_identity_basis = if verified {
        SourceIdentityBasis::VerifiedReleaseAuthority
    } else {
        SourceIdentityBasis::UnverifiedPreinstall
    };
    let primary = BinaryEvidence {
        path: PathBuf::from("/Users/ci/.local/bin/shipyard"),
        semantic_version: version.to_owned(),
        sha256: "a".repeat(64),
        source_identity: source_identity.clone(),
        source_identity_basis,
    };
    let companion = tag_requires_companion(&format!("v{version}")).then(|| BinaryEvidence {
        path: PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider"),
        semantic_version: version.to_owned(),
        sha256: "b".repeat(64),
        source_identity,
        source_identity_basis,
    });
    BinaryPairEvidence { primary, companion }
}

fn evidence(version: &str) -> HostUpdateEvidence {
    HostUpdateEvidence {
        release_authority_identity: verified_source_identity(),
        release_asset_sha256: "6".repeat(64),
        executable_sha256: "a".repeat(64),
        cli_version: format!("shipyard {version}"),
        before_pair: pair(version, false),
        after_pair: pair(version, true),
        auth_support_before: auth_support(false),
        auth_support_after: auth_support(true),
        daemon_version: version.to_owned(),
        daemon_pid: 42,
        daemon_runtime: daemon_runtime(),
        configured_repos_before: Some(vec!["owner/repo".to_owned()]),
        configured_repos_after: vec!["owner/repo".to_owned()],
        configured_repos_preserved: Some(true),
    }
}

fn daemon_runtime() -> DaemonRuntimeEvidence {
    DaemonRuntimeEvidence {
        pid: 42,
        loaded_executable_path: PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations")
            .join("7".repeat(64))
            .join("shipyard"),
        loaded_executable_sha256: "a".repeat(64),
        rendered_launch_sha256: "1".repeat(64),
        loaded_launch_sha256: "1".repeat(64),
        machine_auth_probe_sha256: "2".repeat(64),
        machine_auth_generation_id: "7".repeat(64),
    }
}

fn auth_support(verified: bool) -> AuthSupportEvidence {
    let generation_dir =
        PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations").join("7".repeat(64));
    let file = |path: &str, digest: char, blob: char| SupportFileEvidence {
        path: PathBuf::from(path),
        generation_target: verified.then(|| {
            PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations")
                .join("7".repeat(64))
                .join(Path::new(path).file_name().expect("support file name"))
        }),
        sha256: Some(digest.to_string().repeat(64)),
        mode: Some(if verified { 0o700 } else { 0o755 }),
        source_blob_oid: verified.then(|| blob.to_string().repeat(40)),
        source_identity: verified.then(verified_source_identity),
        source_identity_basis: if verified {
            SourceIdentityBasis::VerifiedReleaseAuthority
        } else {
            SourceIdentityBasis::UnverifiedPreinstall
        },
    };
    AuthSupportEvidence {
        helper: file(
            "/Users/ci/.config/shipyard/bin/shipyard-github-app-token",
            'c',
            'b',
        ),
        wrapper: file("/Users/ci/.local/bin/ghapp", 'e', 'd'),
        generation: verified.then(|| GenerationEvidence {
            generation_contract: "auth-selector-v1".to_owned(),
            generation_id: "7".repeat(64),
            authority_identity: verified_source_identity(),
            selector_path: PathBuf::from("/Users/ci/.local/bin/ghapp"),
            selector_target: generation_dir.join("ghapp"),
            selector_recheck_target: generation_dir.join("ghapp"),
            manifest: generation_member(&generation_dir, "generation.manifest", '9', 0o600),
            helper: generation_member(&generation_dir, "shipyard-github-app-token", 'c', 0o700),
            wrapper: generation_member(&generation_dir, "ghapp", 'e', 0o700),
            binary: generation_member(&generation_dir, "shipyard", 'a', 0o700),
            companion: Some(generation_member(
                &generation_dir,
                COMPANION_BINARY_NAME,
                'b',
                0o700,
            )),
            context: Some(generation_member(
                &generation_dir,
                "ghapp.shipyard-context.json",
                'd',
                0o600,
            )),
        }),
    }
}

fn generation_member(
    generation_dir: &Path,
    name: &str,
    digest: char,
    mode: u32,
) -> GenerationMemberEvidence {
    GenerationMemberEvidence {
        path: generation_dir.join(name),
        sha256: digest.to_string().repeat(64),
        mode,
    }
}

#[test]
fn remote_evidence_is_typed_and_proves_repo_preservation() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let before = serde_json::json!({
        "command": "daemon:status",
        "running": true,
        "configured_repos": ["owner/b", "owner/a"],
        "shipyard_version": "0.126.2"
    });
    let refresh = serde_json::json!({
        "command": "daemon:refresh",
        "new_pid": 4242,
        "repos": ["owner/a", "owner/b"]
    });
    let after = serde_json::json!({
        "command": "daemon:status",
        "running": true,
        "configured_repos": ["owner/a", "owner/b"],
        "shipyard_version": "0.134.0"
    });
    let stdout = format!(
        "{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_BEFORE_PRIMARY_VERSION_PREFIX}shipyard 0.126.2\n{REMOTE_BEFORE_COMPANION_SHA256_PREFIX}absent\n{REMOTE_BEFORE_COMPANION_VERSION_PREFIX}absent\n{REMOTE_AFTER_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_AFTER_PRIMARY_VERSION_PREFIX}shipyard 0.134.0\n{REMOTE_AFTER_COMPANION_SHA256_PREFIX}{}\n{REMOTE_AFTER_COMPANION_VERSION_PREFIX}shipyard-workstream-provider 0.134.0\n{}{}\n{}755\n{}direct\n{}{}\n{}755\n{}direct\n{}{}\n{}700\n{}{generation_root}/shipyard-github-app-token\n{}{}\n{}700\n{}{generation_root}/ghapp\n{REMOTE_GENERATION_SELECTOR_PREFIX}{generation_root}/ghapp\n{REMOTE_GENERATION_SELECTOR_RECHECK_PREFIX}{generation_root}/ghapp\n{REMOTE_GENERATION_ID_PREFIX}{generation_id}\n{REMOTE_GENERATION_CONTRACT_PREFIX}auth-selector-v1\n{REMOTE_GENERATION_AUTHORITY_PREFIX}{authority_identity}\n{REMOTE_GENERATION_MANIFEST_SHA_PREFIX}{manifest_sha}\n{REMOTE_GENERATION_HELPER_SHA_PREFIX}{helper_sha}\n{REMOTE_GENERATION_WRAPPER_SHA_PREFIX}{wrapper_sha}\n{REMOTE_GENERATION_BINARY_SHA_PREFIX}{binary_sha}\n{REMOTE_GENERATION_COMPANION_SHA_PREFIX}{companion_sha}\n{REMOTE_GENERATION_CONTEXT_SHA_PREFIX}{context_sha}\n{REMOTE_DAEMON_PID_PREFIX}4242\n{REMOTE_DAEMON_EXECUTABLE_PREFIX}{generation_root}/shipyard\n{REMOTE_DAEMON_EXECUTABLE_SHA_PREFIX}{binary_sha}\n{REMOTE_DAEMON_LAUNCH_PREFIX}{daemon_launch}\n{REMOTE_DAEMON_AUTH_PROBE_SHA_PREFIX}{auth_probe_sha}\n{REMOTE_BEFORE_STATUS_PREFIX}{before}\n{REMOTE_REFRESH_PREFIX}{refresh}\n{REMOTE_AFTER_STATUS_PREFIX}{after}\n{REMOTE_AUTHORITY_ID_PREFIX}{}\n{REMOTE_RELEASE_ASSET_SHA256_PREFIX}{}\n",
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64),
        auth_support::BEFORE_HELPER_SHA_PREFIX,
        "f".repeat(64),
        auth_support::BEFORE_HELPER_MODE_PREFIX,
        auth_support::BEFORE_HELPER_TARGET_PREFIX,
        auth_support::BEFORE_WRAPPER_SHA_PREFIX,
        "a".repeat(64),
        auth_support::BEFORE_WRAPPER_MODE_PREFIX,
        auth_support::BEFORE_WRAPPER_TARGET_PREFIX,
        auth_support::AFTER_HELPER_SHA_PREFIX,
        "c".repeat(64),
        auth_support::AFTER_HELPER_MODE_PREFIX,
        auth_support::AFTER_HELPER_TARGET_PREFIX,
        auth_support::AFTER_WRAPPER_SHA_PREFIX,
        "e".repeat(64),
        auth_support::AFTER_WRAPPER_MODE_PREFIX,
        auth_support::AFTER_WRAPPER_TARGET_PREFIX,
        "8".repeat(64),
        "6".repeat(64),
        generation_root = format!(
            "/Users/ci/.local/share/shipyard/auth-generations/{}",
            "7".repeat(64)
        ),
        generation_id = "7".repeat(64),
        authority_identity = "8".repeat(64),
        manifest_sha = "9".repeat(64),
        helper_sha = "c".repeat(64),
        wrapper_sha = "e".repeat(64),
        binary_sha = "b".repeat(64),
        companion_sha = "c".repeat(64),
        context_sha = "d".repeat(64),
        daemon_launch = "/Users/ci/.local/bin/shipyard --mode shipyard --global-dir /Users/ci/Library/Application Support/shipyard --state-dir /Users/ci/Library/Application Support/shipyard daemon run --repo owner/a --repo owner/b",
        auth_probe_sha = "5".repeat(64),
    );
    let evidence = parse_remote_evidence(&plan, stdout.as_bytes()).expect("evidence");
    assert_eq!(evidence.daemon_pid, 4242);
    assert_eq!(evidence.configured_repos_preserved, Some(true));
    validate_evidence(&plan, &evidence).expect("valid evidence");
}

#[test]
fn evidence_rejects_version_repo_and_digest_drift() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let mut observed = evidence("0.134.0");
    observed.daemon_version = "0.126.2".to_owned();
    assert!(validate_evidence(&plan, &observed).is_err());
    observed.daemon_version = "0.134.0".to_owned();
    observed.configured_repos_preserved = Some(false);
    assert!(validate_evidence(&plan, &observed).is_err());
    observed.configured_repos_preserved = Some(true);
    observed.executable_sha256 = "d".repeat(64);
    assert!(validate_evidence(&plan, &observed).is_err());
    observed.executable_sha256 = "a".repeat(64);
    observed.release_authority_identity = "9".repeat(64);
    assert!(validate_evidence(&plan, &observed).is_err());
    observed.release_authority_identity = "8".repeat(64);
    observed.release_asset_sha256 = "5".repeat(64);
    assert!(validate_evidence(&plan, &observed).is_err());
    assert!(!valid_sha256(&"A".repeat(64)));
    assert!(!valid_sha256("short"));
}

#[test]
fn daemon_runtime_receipt_refuses_pid_binary_launch_and_auth_generation_drift() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let control = evidence("0.134.0");
    validate_evidence(&plan, &control).expect("coherent daemon runtime control");

    let mut pid = control.clone();
    pid.daemon_runtime.pid += 1;
    assert!(validate_evidence(&plan, &pid).is_err());

    let mut binary = control.clone();
    binary.daemon_runtime.loaded_executable_sha256 = "f".repeat(64);
    assert!(validate_evidence(&plan, &binary).is_err());

    let mut launch = control.clone();
    launch.daemon_runtime.loaded_launch_sha256 = "f".repeat(64);
    assert!(validate_evidence(&plan, &launch).is_err());

    let mut auth = control;
    auth.daemon_runtime.machine_auth_generation_id = "f".repeat(64);
    assert!(validate_evidence(&plan, &auth).is_err());
}

#[test]
fn auth_support_evidence_rejects_tamper_and_mixed_release_source() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let mut observed = evidence("0.134.0");
    observed.auth_support_after.helper.sha256 = Some("f".repeat(64));
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed.auth_support_after.wrapper.source_blob_oid = Some("b".repeat(40));
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed.auth_support_after.wrapper.mode = Some(0o755);
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed.auth_support_after.helper.generation_target = None;
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed.auth_support_after.wrapper.generation_target = Some(
        PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations")
            .join("6".repeat(64))
            .join("ghapp"),
    );
    assert!(validate_evidence(&plan, &observed).is_err());
}

#[test]
fn composed_generation_rejects_selector_toctou_and_mixed_members() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let mut observed = evidence("0.134.0");
    let generation = observed
        .auth_support_after
        .generation
        .as_mut()
        .expect("generation");
    generation.selector_recheck_target = generation
        .selector_target
        .parent()
        .expect("generation dir")
        .parent()
        .expect("generation root")
        .join("6".repeat(64))
        .join("ghapp");
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed
        .auth_support_after
        .generation
        .as_mut()
        .expect("generation")
        .binary
        .sha256 = "f".repeat(64);
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed
        .auth_support_after
        .generation
        .as_mut()
        .expect("generation")
        .authority_identity = "6".repeat(64);
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed
        .auth_support_after
        .generation
        .as_mut()
        .expect("generation")
        .companion
        .as_mut()
        .expect("companion")
        .sha256 = "f".repeat(64);
    assert!(validate_evidence(&plan, &observed).is_err());
}

#[test]
fn generation_target_shape_requires_canonical_absolute_exact_member() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let root = PathBuf::from("/Users/ci/.local/share/shipyard/auth-generations");
    let generation = "7".repeat(64);
    let expected = root.join(&generation).join("ghapp");
    assert_eq!(
        validate_generation_target_shape(&plan.auth_wrapper, &expected, &plan).expect("target"),
        root.join(&generation)
    );

    for invalid in [
        PathBuf::from("relative/auth-generations")
            .join(&generation)
            .join("ghapp"),
        root.join("7".repeat(63)).join("ghapp"),
        root.join("A".repeat(64)).join("ghapp"),
        root.join(&generation).join("wrong-name"),
        root.join(&generation).join("extra").join("ghapp"),
        root.join(&generation)
            .join("..")
            .join(&generation)
            .join("ghapp"),
        PathBuf::from("/tmp").join(&generation).join("ghapp"),
    ] {
        assert!(
            validate_generation_target_shape(&plan.auth_wrapper, &invalid, &plan).is_err(),
            "accepted malformed target {}",
            invalid.display()
        );
    }
}

#[cfg(unix)]
#[test]
fn local_generation_target_requires_private_owned_regular_member() {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path().join("home");
    let bin = home.join(".local/bin");
    let helper_dir = home.join(".config/shipyard/bin");
    std::fs::create_dir_all(&bin).expect("bin");
    std::fs::create_dir_all(&helper_dir).expect("helper dir");
    let mut class = host(None);
    class.shipyard_bin = Some(bin.join("shipyard").display().to_string());
    class.github_cli = Some(bin.join("ghapp").display().to_string());
    class.github_token_helper = Some(
        helper_dir
            .join("shipyard-github-app-token")
            .display()
            .to_string(),
    );
    let plan = super::super::host_update_plan(&class, "v0.134.0").expect("plan");
    let generation_dir = home
        .join(".local/share/shipyard/auth-generations")
        .join("7".repeat(64));
    std::fs::create_dir_all(&generation_dir).expect("generation dir");
    for private_dir in [
        home.join(".local/share/shipyard"),
        home.join(".local/share/shipyard/auth-generations"),
    ] {
        std::fs::set_permissions(&private_dir, std::fs::Permissions::from_mode(0o700))
            .expect("private generation root");
    }
    std::fs::set_permissions(&generation_dir, std::fs::Permissions::from_mode(0o700))
        .expect("private generation dir");
    let target = generation_dir.join("ghapp");
    std::fs::write(&target, "#!/bin/sh\n").expect("target");
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).expect("target mode");
    symlink(&target, &plan.auth_wrapper).expect("canonical link");

    let observed = collect_local_support_file(&plan.auth_wrapper, Some("d"), true, &plan)
        .expect("valid generation target");
    assert_eq!(
        observed.generation_target.as_deref(),
        Some(target.as_path())
    );

    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
        .expect("unsafe target mode");
    assert!(
        collect_local_support_file(&plan.auth_wrapper, Some("d"), true, &plan).is_err(),
        "accepted unsafe target mode"
    );

    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700))
        .expect("restore target mode");
    std::fs::set_permissions(&generation_dir, std::fs::Permissions::from_mode(0o755))
        .expect("unsafe generation mode");
    assert!(
        collect_local_support_file(&plan.auth_wrapper, Some("d"), true, &plan).is_err(),
        "accepted unsafe generation mode"
    );

    std::fs::set_permissions(&generation_dir, std::fs::Permissions::from_mode(0o700))
        .expect("restore generation mode");
    let share_root = home.join(".local/share");
    std::fs::set_permissions(&share_root, std::fs::Permissions::from_mode(0o777))
        .expect("unsafe share ancestor mode");
    assert!(
        collect_local_support_file(&plan.auth_wrapper, Some("d"), true, &plan).is_err(),
        "accepted a group/world-writable generation ancestor"
    );
    std::fs::set_permissions(&share_root, std::fs::Permissions::from_mode(0o755))
        .expect("restore share ancestor mode");
    std::fs::remove_file(&target).expect("remove regular target");
    let indirect_target = generation_dir.join("indirect-ghapp");
    std::fs::write(&indirect_target, "#!/bin/sh\n").expect("indirect target");
    std::fs::set_permissions(&indirect_target, std::fs::Permissions::from_mode(0o700))
        .expect("indirect target mode");
    symlink(&indirect_target, &target).expect("nested target link");
    assert!(
        collect_local_support_file(&plan.auth_wrapper, Some("d"), true, &plan).is_err(),
        "accepted a generation member that was itself a symlink"
    );
}

#[test]
fn evidence_never_accepts_mixed_pair_or_legacy_companion() {
    let paired_plan =
        super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("paired plan");
    let mut mixed = evidence("0.134.0");
    mixed
        .after_pair
        .companion
        .as_mut()
        .expect("companion")
        .semantic_version = "0.126.4".to_owned();
    assert!(validate_evidence(&paired_plan, &mixed).is_err());

    let mut wrong_source = evidence("0.134.0");
    wrong_source.after_pair.primary.source_identity = Some("9".repeat(64));
    assert!(validate_evidence(&paired_plan, &wrong_source).is_err());

    let mut legacy_mixed = evidence("0.126.2");
    legacy_mixed.after_pair.companion = Some(BinaryEvidence {
        path: paired_plan.companion_binary.clone(),
        semantic_version: "0.126.2".to_owned(),
        sha256: "d".repeat(64),
        source_identity: Some(verified_source_identity()),
        source_identity_basis: SourceIdentityBasis::VerifiedReleaseAuthority,
    });
    assert!(
        validate_binary_pair(&paired_plan, &legacy_mixed.after_pair, Some("v0.126.2")).is_err()
    );
}

#[test]
fn evidence_never_infers_preinstall_provenance_or_omits_postinstall_binding() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");

    let mut fabricated_before = evidence("0.134.0");
    fabricated_before.before_pair.primary.source_identity = Some(verified_source_identity());
    fabricated_before.before_pair.primary.source_identity_basis =
        SourceIdentityBasis::VerifiedReleaseAuthority;
    fabricated_before
        .before_pair
        .companion
        .as_mut()
        .expect("companion")
        .source_identity = Some(verified_source_identity());
    fabricated_before
        .before_pair
        .companion
        .as_mut()
        .expect("companion")
        .source_identity_basis = SourceIdentityBasis::VerifiedReleaseAuthority;
    assert!(validate_evidence(&plan, &fabricated_before).is_err());

    let mut unbound_after = evidence("0.134.0");
    unbound_after.after_pair.primary.source_identity = None;
    unbound_after.after_pair.primary.source_identity_basis =
        SourceIdentityBasis::UnverifiedPreinstall;
    unbound_after
        .after_pair
        .companion
        .as_mut()
        .expect("companion")
        .source_identity = None;
    unbound_after
        .after_pair
        .companion
        .as_mut()
        .expect("companion")
        .source_identity_basis = SourceIdentityBasis::UnverifiedPreinstall;
    assert!(validate_evidence(&plan, &unbound_after).is_err());
}

#[test]
fn fresh_daemon_reports_preservation_as_not_applicable() {
    let before = serde_json::json!({"command": "daemon:status", "running": false});
    let after = serde_json::json!({
        "command": "daemon:status",
        "running": true,
        "configured_repos": [],
        "shipyard_version": "0.134.0"
    });
    let pair = evidence("0.134.0").after_pair;
    let observed = evidence_from_values(
        pair.clone(),
        pair,
        auth_support(false),
        auth_support(true),
        9,
        daemon_runtime(),
        &before,
        &after,
        verified_source_identity(),
        "6".repeat(64),
    )
    .expect("fresh evidence");
    assert_eq!(observed.configured_repos_before, None);
    assert_eq!(observed.configured_repos_preserved, None);
}

#[test]
fn remote_evidence_rejects_duplicate_or_incomplete_markers() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.134.0").expect("plan");
    let duplicate = format!(
        "{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n",
        "a".repeat(64),
        "b".repeat(64)
    );
    assert!(parse_remote_evidence(&plan, duplicate.as_bytes()).is_err());
    assert!(parse_remote_evidence(&plan, b"").is_err());
}

#[cfg(unix)]
#[test]
fn local_evidence_probes_share_the_host_attempt_deadline() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let binary = temp.path().join("shipyard");
    std::fs::write(
        &binary,
        "#!/bin/sh\ncase \"$*\" in *\"daemon status\"*) printf '%s\\n' '{\"command\":\"daemon:status\",\"running\":false}' ;; *\"--version\"*) sleep 60 ;; *) printf '%s\\n' '{\"command\":\"daemon:refresh\",\"new_pid\":42}' ;; esac\n",
    )
    .expect("fixture");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("executable");
    let mut class = host(None);
    class.shipyard_bin = Some(binary.display().to_string());
    class.github_cli = Some(temp.path().join("ghapp").display().to_string());
    let plan = super::super::host_update_plan(&class, "v0.134.0").expect("plan");
    let started = Instant::now();
    assert!(matches!(
        execute_plan_with_timeout(&plan, Duration::from_millis(100)),
        Err(PlanExecutionError::TimedOut(_))
    ));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "evidence probe received a fresh timeout after the host deadline"
    );
}

#[cfg(unix)]
#[test]
fn remote_supervisor_kills_term_ignoring_descendants_after_leader_exits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let pid_file = temp.path().join("descendant.pid");
    let worker = format!(
        "(trap '' TERM; echo $$ > {}; while :; do sleep 1; done) & wait $!",
        crate::executor::ssh::shlex_quote(&pid_file.display().to_string())
    );
    let status = Command::new("/usr/bin/perl")
        .args([
            "-e",
            super::super::REMOTE_SUPERVISOR,
            "1",
            "/bin/bash",
            "-c",
            &worker,
        ])
        .status()
        .expect("remote supervisor fixture");
    assert_eq!(status.code(), Some(124));

    let pid = std::fs::read_to_string(pid_file)
        .expect("descendant pid")
        .trim()
        .to_owned();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline
        && Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !Command::new("/bin/kill")
            .args(["-0", &pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success()),
        "TERM-ignoring descendant survived the remote timeout boundary"
    );
}
