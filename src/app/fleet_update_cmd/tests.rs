#[cfg(unix)]
use std::process::Command;

use super::*;

fn host(ssh: Option<&str>, shipyard_bin: Option<&str>) -> HostClassConfig {
    let github_cli = shipyard_bin
        .and_then(|binary| Path::new(binary).parent())
        .map_or_else(
            || "/Users/ci/.local/bin/ghapp".to_owned(),
            |parent| parent.join("ghapp").display().to_string(),
        );
    HostClassConfig {
        class: "m5".to_owned(),
        ssh: ssh.map(str::to_owned),
        cap: 2,
        tart_bin: "/opt/homebrew/bin/tart".to_owned(),
        tartci_bin: "/Users/ci/.local/bin/tartci".to_owned(),
        shipyard_bin: shipyard_bin.map(str::to_owned),
        shipyard_mode: Some("shipyard".to_owned()),
        shipyard_global_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
        shipyard_state_dir: Some("/Users/ci/Library/Application Support/shipyard".to_owned()),
        github_cli: Some(github_cli),
        github_token_helper: Some(
            "/Users/ci/.config/shipyard/bin/shipyard-github-app-token".to_owned(),
        ),
        tart_home: Some("/Users/ci/VMs".to_owned()),
        labels: Vec::new(),
    }
}

#[test]
fn remote_plan_uses_absolute_binary_and_minimal_canonical_path() {
    let plan = host_update_plan(
        &host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard")),
        "v0.127.0",
    )
    .expect("plan");
    assert!(plan.command.starts_with("/usr/bin/env -i HOME=\"$HOME\""));
    assert!(plan.command.contains("/opt/homebrew/bin"));
    assert!(plan.command.contains("/usr/bin/perl -e"));
    assert!(plan.command.contains("TERM"));
    assert!(plan.command.contains("KILL"));
    assert!(plan.command.contains("waitpid"));
    assert!(plan.command.contains(&format!(
        " {} /bin/bash -c",
        REMOTE_UPDATE_TIMEOUT.as_secs()
    )));
    assert!(plan.command.contains("/Users/ci/.local/bin/shipyard"));
    assert!(plan.command.contains("/Users/ci/.local/bin/ghapp"));
    assert!(plan.command.contains(
        "test /Users/ci/.config/shipyard/bin/shipyard-github-app-token = \"$HOME/.config/shipyard/bin/shipyard-github-app-token\""
    ));
    assert!(
        plan.command
            .contains(&format!("Shipyard/{}/install.sh", "2".repeat(40)))
    );
    assert!(plan.command.contains("releases/assets/11"));
    assert!(plan.command.contains(&"a".repeat(64)));
    assert!(plan.command.contains(&"6".repeat(64)));
    assert!(plan.command.contains("--mode shipyard"));
    assert!(
        plan.command
            .contains("/Users/ci/Library/Application Support/shipyard")
    );
    assert!(
        plan.command
            .contains("update --to v0.127.0 --check --unattended-fleet")
    );
    assert_eq!(
        plan.companion_binary,
        PathBuf::from("/Users/ci/.local/bin/shipyard-workstream-provider")
    );
    assert!(plan.companion_required);
    assert_eq!(plan.source_identity, "8".repeat(64));
    assert!(plan.command.contains("/usr/bin/shasum -a 256"));
    assert!(plan.command.contains(REMOTE_BEFORE_STATUS_PREFIX));
    assert!(plan.command.contains(REMOTE_REFRESH_PREFIX));
    assert!(plan.command.contains(REMOTE_AFTER_STATUS_PREFIX));
    assert!(plan.command.contains(REMOTE_AUTHORITY_ID_PREFIX));
    assert!(plan.command.contains(REMOTE_RELEASE_ASSET_SHA256_PREFIX));
    let status_probe = plan.command.find("before_status=").expect("status probe");
    let auth = plan.command.find("token=").expect("auth boundary");
    assert!(
        status_probe < auth,
        "status must fail before auth and install"
    );
    assert!(!plan.command.contains("observed_before="));
    let preflight = plan
        .command
        .find("staged_binary")
        .expect("staged authenticated preflight");
    let replacement = plan
        .command
        .find("SHIPYARD_INSTALL_DIR=/Users/ci/.local/bin")
        .expect("real install destination");
    assert!(
        preflight < replacement,
        "governed config and helper must pass before binary replacement"
    );
}

#[test]
fn fleet_plan_requires_ghapp_and_shipyard_to_be_siblings() {
    let mut class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    class.github_cli = Some("/Users/ci/bin/ghapp".to_owned());
    let error = host_update_plan(&class, "v0.127.0").expect_err("foreign wrapper directory");
    assert!(error.message.contains("ghapp sibling of shipyard_bin"));

    class.github_cli = Some("/Users/ci/.local/bin/renamed-ghapp".to_owned());
    let error = host_update_plan(&class, "v0.127.0").expect_err("foreign wrapper name");
    assert!(error.message.contains("ghapp sibling of shipyard_bin"));
}

#[test]
fn fleet_resolver_probe_uses_exact_global_dir_before_commit() {
    let mut class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    class.shipyard_global_dir = Some("/Users/ci/governed global".to_owned());
    class.shipyard_state_dir = Some("/Users/ci/governed state".to_owned());

    let legacy = host_update_plan(&class, "v0.128.9").expect("legacy target");
    assert!(legacy.command.contains("auth_resolver_required=0"));

    let plan = host_update_plan(&class, "v0.129.0").expect("differing governed dirs");
    assert!(plan.command.contains("auth_resolver_required=1"));

    assert!(plan.command.contains("/Users/ci/governed global"));
    assert!(plan.command.contains("/Users/ci/governed state"));
    assert!(plan.command.contains("$auth_wrapper.shipyard-context.json"));
    assert!(
        plan.command
            .contains("--global-dir \"$auth_global_dir\" auth helper-argv")
    );
    let probe = plan
        .command
        .find("auth helper-argv")
        .expect("resolver probe");
    let committed = plan
        .command
        .find("auth_write_phase committed")
        .expect("commit marker");
    assert!(
        probe < committed,
        "resolver must pass before transaction commit"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn remote_pair_probe_rejects_mixed_or_malformed_preinstall_state() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let primary = temp.path().join("shipyard");
    let companion = temp.path().join(COMPANION_BINARY_NAME);
    let write_binary = |path: &Path, label: &str, version: &str| {
        std::fs::write(
            path,
            format!("#!/bin/sh\nprintf '%s\\n' '{label} {version}'\n"),
        )
        .expect("fixture");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("executable");
    };
    write_binary(&primary, "shipyard", "0.126.2");
    let legacy_probe = remote_pair_probe(&primary, &companion, "before", None, false);
    assert!(
        Command::new("/bin/bash")
            .args(["-c", &legacy_probe])
            .status()
            .expect("legacy probe")
            .success()
    );

    write_binary(&companion, COMPANION_BINARY_NAME, "0.127.0");
    assert!(
        !Command::new("/bin/bash")
            .args(["-c", &legacy_probe])
            .status()
            .expect("mixed probe")
            .success()
    );

    write_binary(&primary, "shipyard", "0.127.0");
    let paired_probe = remote_pair_probe(&primary, &companion, "before", None, false);
    assert!(
        Command::new("/bin/bash")
            .args(["-c", &paired_probe])
            .status()
            .expect("paired probe")
            .success()
    );

    for malformed in [
        "0.126.",
        "0.0126.3",
        "0.127.0.1",
        "18446744073709551616.0.0",
    ] {
        write_binary(&primary, "shipyard", malformed);
        write_binary(&companion, COMPANION_BINARY_NAME, malformed);
        let malformed_probe = remote_pair_probe(&primary, &companion, "before", None, false);
        assert!(
            !Command::new("/bin/bash")
                .args(["-c", &malformed_probe])
                .status()
                .expect("malformed probe")
                .success(),
            "malformed preinstall version {malformed:?} must fail before rollout"
        );
    }
}

#[cfg(unix)]
#[test]
fn local_plan_preserves_host_class_daemon_context() {
    let mut class = host(None, Some("/Users/ci/.local/bin/shipyard"));
    class.shipyard_mode = Some("isolated".to_owned());
    class.shipyard_global_dir = Some("/tmp/governed config".to_owned());
    class.shipyard_state_dir = Some("/tmp/governed state".to_owned());
    let plan = host_update_plan(&class, "v0.98.1").expect("plan");
    let command = local_update_command(&plan);

    assert!(command.contains("--mode isolated"));
    assert!(command.contains("--global-dir '/tmp/governed config'"));
    assert!(command.contains("--state-dir '/tmp/governed state'"));
    assert!(command.contains("--unattended-fleet"));
    assert!(!command.contains("--refresh-daemon"));
    let resolver_probe = command
        .find("auth helper-argv --wrapper")
        .expect("resolver probe");
    let committed = command
        .find("auth_write_phase committed")
        .expect("transaction commit");
    let daemon_refresh = command
        .find("--json daemon refresh")
        .expect("daemon refresh");
    let lock_release = command.rfind("auth_release_lock").expect("lock release");
    assert_eq!(command.matches("--json daemon refresh").count(), 1);
    assert!(command.contains("--json daemon refresh 9>&-"));
    assert!(!command[..committed].contains("--json daemon refresh"));
    assert!(resolver_probe < committed);
    assert!(committed < daemon_refresh);
    assert!(daemon_refresh < lock_release);
    assert!(command.contains("fleet-auth-support.transaction"));
}

#[cfg(unix)]
#[test]
fn exact_asset_shim_serves_authenticated_and_unauthenticated_installer_urls() {
    let temp = tempfile::tempdir().expect("temp");
    let shim = temp.path().join("curl-shim");
    std::fs::write(&shim, exact_asset_curl_shim("shipyard-macos-arm64")).expect("shim");
    let asset = temp.path().join("verified-asset");
    let output = Command::new("/bin/bash")
        .arg(&shim)
        .arg("https://api.github.com/repos/example/shipyard/releases/tags/v1.2.3")
        .env("SHIPYARD_FLEET_ASSET_PATH", &asset)
        .output()
        .expect("run shim");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 response");
    let payload: Value =
        serde_json::from_str(stdout.lines().next().expect("release response JSON line"))
            .expect("release response JSON");
    let expected = format!("file://{}", asset.display());
    assert_eq!(
        payload.pointer("/assets/0/url").and_then(Value::as_str),
        Some(expected.as_str())
    );
    assert_eq!(
        payload
            .pointer("/assets/0/browser_download_url")
            .and_then(Value::as_str),
        Some(expected.as_str())
    );
}

#[test]
fn stripped_path_is_launch_environment_drift_not_absence() {
    let error = host_update_plan(&host(Some("m5"), None), "v0.98.1")
        .expect_err("remote relative lookup must fail closed");
    assert!(error.message.contains("launch-environment drift"));
    assert!(!error.message.contains("not installed"));
    assert!(!error.message.contains("absent"));
}

#[test]
fn option_like_ssh_destination_is_rejected_before_spawn() {
    let error = host_update_plan(
        &host(
            Some("-oProxyCommand=/tmp/untrusted"),
            Some("/Users/ci/.local/bin/shipyard"),
        ),
        "v0.98.1",
    )
    .expect_err("SSH option injection");
    assert!(error.message.contains("not a valid SSH destination"));
}

#[test]
fn auth_support_paths_reject_dot_and_parent_components() {
    let mut class = host(Some("m5"), Some("/Users/ci/.local/bin/shipyard"));
    class.github_token_helper =
        Some("/Users/ci/.config/shipyard/bin/../shipyard-github-app-token".to_owned());
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("parent component must fail before command construction");
    assert!(
        error
            .message
            .contains("must not contain dot or parent components")
    );

    class.github_token_helper =
        Some("/Users/ci/.config/shipyard/./bin/shipyard-github-app-token".to_owned());
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("dot component must fail before command construction");
    assert!(
        error
            .message
            .contains("must not contain dot or parent components")
    );
}

#[test]
fn daemon_context_paths_reject_controls_dot_and_parent_components() {
    for global_dir in ["/Users/ci/global/../foreign", "/Users/ci/global\nforeign"] {
        let mut class = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
        class.shipyard_global_dir = Some(global_dir.to_owned());
        let error = host_update_plan(&class, "v0.127.0").expect_err("unsafe global dir");
        assert!(error.message.contains("normalized absolute paths"));
    }
}

#[test]
fn auth_support_paths_reject_managed_binary_and_transaction_collisions() {
    let mut class = host(Some("m5"), Some("/Users/ci/.local/bin/shipyard"));
    class.github_token_helper = Some("/Users/ci/.local/bin/shipyard".to_owned());
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("primary binary collision must fail before rollout");
    assert!(error.message.contains("must not overlap managed binaries"));

    class.github_token_helper =
        Some("/Users/ci/.local/bin/shipyard-workstream-provider".to_owned());
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("companion binary collision must fail before rollout");
    assert!(error.message.contains("must not overlap managed binaries"));

    class.github_token_helper =
        Some("/Users/ci/.local/bin/shipyard.shipyard-rollback.tmp".to_owned());
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("atomic backup temp collision must fail before rollout");
    assert!(error.message.contains("must not overlap managed binaries"));

    class.github_token_helper = Some(
        "/Users/ci/Library/Application Support/shipyard/fleet-auth-support.transaction".to_owned(),
    );
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("journal collision must fail before rollout");
    assert!(error.message.contains("or transaction state"));

    class.github_token_helper =
        Some("/Users/ci/Library/Application Support/shipyard/fleet-auth-support.guard".to_owned());
    let error = host_update_plan(&class, "v0.127.0")
        .expect_err("advisory guard collision must fail before rollout");
    assert!(error.message.contains("or transaction state"));
}

#[test]
fn remote_bootstrap_requires_absolute_governed_auth_helper() {
    let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    config.github_cli = Some("ghapp".to_owned());
    let error = host_update_plan(&config, "v0.98.1").expect_err("relative helper");
    assert!(
        error
            .message
            .contains("auth helper and wrapper paths must be distinct absolute paths")
    );
}

#[test]
fn remote_rollout_requires_an_explicit_daemon_context() {
    let mut config = host(Some("m5-lan"), Some("/Users/ci/.local/bin/shipyard"));
    config.shipyard_state_dir = None;
    let error = host_update_plan(&config, "v0.100.0").expect_err("missing context");
    assert!(
        error
            .message
            .contains("shipyard_state_dir is required to identify the daemon context")
    );
}

#[test]
fn remote_bootstrap_rejects_a_filename_the_installer_cannot_replace() {
    let error = host_update_plan(
        &host(Some("m5-lan"), Some("/Users/ci/.local/bin/current")),
        "v0.98.1",
    )
    .expect_err("renamed binary");
    assert!(error.message.contains("must end in /shipyard"));
}

#[cfg(unix)]
#[test]
fn local_rollout_rejects_a_filename_the_installer_cannot_replace() {
    let error = host_update_plan(&host(None, Some("/Users/ci/.local/bin/current")), "v0.98.1")
        .expect_err("renamed local binary");
    assert!(error.message.contains("must end in /shipyard"));
}

#[cfg(unix)]
#[test]
fn one_stalled_host_is_terminated_at_its_bound() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let binary = temp.path().join("shipyard");
    std::fs::write(
        &binary,
        "#!/bin/sh\ncase \"$*\" in *\"daemon status\"*) printf '%s\\n' '{\"command\":\"daemon:status\",\"running\":false}' ;; *) sleep 60 ;; esac\n",
    )
    .expect("fixture");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("executable");
    let plan = host_update_plan(&host(None, binary.to_str()), "v0.98.1").expect("plan");
    assert!(matches!(
        execute_plan_with_timeout(&plan, Duration::from_millis(50)),
        Err(PlanExecutionError::TimedOut(_))
    ));
}

#[cfg(unix)]
#[test]
fn configured_absolute_tool_survives_a_stripped_non_login_path() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let homebrew_bin = temp.path().join("opt/homebrew/bin");
    std::fs::create_dir_all(&homebrew_bin).expect("homebrew bin");
    let tart = homebrew_bin.join("tart");
    std::fs::write(&tart, "#!/bin/sh\nexit 0\n").expect("tool fixture");
    std::fs::set_permissions(&tart, std::fs::Permissions::from_mode(0o755))
        .expect("executable fixture");
    let shipyard = homebrew_bin.join("shipyard");
    std::fs::write(&shipyard, "#!/bin/sh\nexit 0\n").expect("Shipyard fixture");
    std::fs::set_permissions(&shipyard, std::fs::Permissions::from_mode(0o755))
        .expect("executable Shipyard fixture");

    let hidden = Command::new("/usr/bin/env")
        .args([
            "-i",
            "PATH=/usr/bin:/bin",
            "/bin/sh",
            "-c",
            "command -v tart",
        ])
        .output()
        .expect("stripped-path probe");
    assert!(
        !hidden.status.success(),
        "ambient lookup must miss the fixture"
    );

    let plan = host_update_plan(&host(Some("m5-lan"), shipyard.to_str()), "v0.98.1")
        .expect("an absolute profile path remains authoritative");
    assert_eq!(plan.binary, shipyard);
    assert!(
        plan.command
            .contains(&shlex_quote(&shipyard.display().to_string()))
    );
}

#[test]
fn exact_release_tag_is_required() {
    assert_eq!(normalize_exact_tag("0.100.0").expect("tag"), "v0.100.0");
    assert!(normalize_exact_tag("v0.99.0").is_err());
    assert!(normalize_exact_tag("v0.98.1").is_err());
    assert!(normalize_exact_tag("latest").is_err());
    assert!(normalize_exact_tag("v0.98").is_err());
    assert!(normalize_exact_tag("v0.98.1-rc1").is_err());
    assert!(normalize_exact_tag("v18446744073709551616.0.0").is_err());
    assert!(!tag_requires_companion("v0.126.2"));
    assert!(tag_requires_companion("v0.127.0"));
    assert!(!tag_supports_auth_resolver("v0.128.9"));
    assert!(tag_supports_auth_resolver("v0.129.0"));
}

fn named_host(name: &str) -> HostClassConfig {
    let mut class = host(Some(name), Some("/Users/ci/.local/bin/shipyard"));
    name.clone_into(&mut class.class);
    class
}

fn pair(version: &str, verified: bool) -> BinaryPairEvidence {
    let source_identity = verified.then(|| "8".repeat(64));
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
        release_authority_identity: "8".repeat(64),
        release_asset_sha256: "6".repeat(64),
        executable_sha256: "a".repeat(64),
        cli_version: format!("shipyard {version}"),
        before_pair: pair(version, false),
        after_pair: pair(version, true),
        auth_support_before: auth_support(false),
        auth_support_after: auth_support(true),
        daemon_version: version.to_owned(),
        daemon_pid: 42,
        configured_repos_before: Some(vec!["owner/repo".to_owned()]),
        configured_repos_after: vec!["owner/repo".to_owned()],
        configured_repos_preserved: Some(true),
    }
}

fn auth_support(verified: bool) -> AuthSupportEvidence {
    let file = |path: &str, digest: char, blob: char| SupportFileEvidence {
        path: PathBuf::from(path),
        sha256: Some(digest.to_string().repeat(64)),
        mode: Some(if verified { 0o700 } else { 0o755 }),
        source_blob_oid: verified.then(|| blob.to_string().repeat(40)),
        source_identity: verified.then(|| "8".repeat(64)),
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
    }
}

#[test]
fn host_selection_is_explicit_ordered_and_fail_closed() {
    let classes = vec![named_host("m1"), named_host("m3"), named_host("m5")];
    let error = select_host_classes(&classes, &[], false).expect_err("implicit fleet");
    assert!(error.message.contains("explicit --all-hosts"));

    let selected = select_host_classes(&classes, &["m5".to_owned(), "m1".to_owned()], false)
        .expect("selected subset");
    assert_eq!(
        selected
            .iter()
            .map(|class| class.class.as_str())
            .collect::<Vec<_>>(),
        ["m5", "m1"]
    );

    let duplicate = select_host_classes(&classes, &["m1".to_owned(), "m1".to_owned()], false)
        .expect_err("duplicate");
    assert!(duplicate.message.contains("more than once"));
    let unknown =
        select_host_classes(&classes, &["studio".to_owned()], false).expect_err("unknown");
    assert!(unknown.message.contains("configured classes: m1, m3, m5"));
    assert!(select_host_classes(&classes, &["m1".to_owned()], true).is_err());
    assert_eq!(
        select_host_classes(&classes, &[], true)
            .expect("explicit all")
            .len(),
        3
    );
}

#[test]
fn apply_stops_before_every_later_host_after_first_failure() {
    let plans = ["m1", "m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.100.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let error = apply_plans(&plans, "v0.100.0", true, &mut output, |plan| {
        attempted.push(plan.class.clone());
        if plan.class == "m3" {
            Err(PlanExecutionError::Failed("controlled failure".to_owned()))
        } else {
            Ok(evidence("0.100.0"))
        }
    })
    .expect_err("apply stops");
    assert_eq!(attempted, ["m1", "m3"]);
    assert!(error.message.contains("stopped after m3"));
    let rendered = String::from_utf8(output).expect("UTF-8");
    let receipts = serde_json::Deserializer::from_str(&rendered)
        .into_iter::<Value>()
        .collect::<Result<Vec<_>, _>>()
        .expect("typed receipts");
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts[0]["host_class"], "m1");
    assert_eq!(receipts[0]["target"], "v0.100.0");
    assert_eq!(receipts[0]["executable_sha256"], "a".repeat(64));
    assert_eq!(
        receipts[0]["binary_pair_before"]["primary"]["source_identity"],
        Value::Null
    );
    assert_eq!(
        receipts[0]["binary_pair_before"]["primary"]["source_identity_basis"],
        "unverified_preinstall"
    );
    assert_eq!(receipts[0]["binary_pair_after"]["companion"], Value::Null);
    assert_eq!(receipts[0]["daemon_pid"], 42);
    assert_eq!(receipts[0]["configured_repos_preserved"], true);
    assert_eq!(receipts[1]["host_class"], "m3");
    assert_eq!(receipts[1]["ok"], false);
    assert!(!rendered.contains("\"host_class\": \"m5\""));
}

#[test]
fn authority_receipt_mismatch_stops_before_the_next_host() {
    let plans = ["m1", "m3"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.127.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let error = apply_plans(&plans, "v0.127.0", true, &mut output, |plan| {
        attempted.push(plan.class.clone());
        let mut observed = evidence("0.127.0");
        if plan.class == "m3" {
            observed.release_authority_identity = "f".repeat(64);
        }
        Ok(observed)
    })
    .expect_err("drift must stop rollout");
    assert_eq!(attempted, ["m1", "m3"]);
    assert!(error.message.contains("stopped after m3 evidence failed"));
    assert!(error.message.contains("frozen release authority"));
}

#[test]
fn cross_host_binary_pair_hash_drift_stops_rollout() {
    let plans = ["m1", "m3", "m5"]
        .iter()
        .map(|name| host_update_plan(&named_host(name), "v0.127.0").expect("plan"))
        .collect::<Vec<_>>();
    let mut attempted = Vec::new();
    let mut output = Vec::new();
    let error = apply_plans(&plans, "v0.127.0", true, &mut output, |plan| {
        attempted.push(plan.class.clone());
        let mut observed = evidence("0.127.0");
        if plan.class == "m3" {
            observed.after_pair.primary.sha256 = "d".repeat(64);
            observed.executable_sha256 = "d".repeat(64);
        }
        Ok(observed)
    })
    .expect_err("cross-host drift must stop rollout");
    assert_eq!(attempted, ["m1", "m3"]);
    assert!(error.message.contains("hashes disagreed"));
}

#[test]
fn paired_host_receipt_exposes_reconcilable_before_and_after_identities() {
    let plan = host_update_plan(&named_host("m1"), "v0.127.0").expect("plan");
    let evidence = evidence("0.127.0");
    let mut output = Vec::new();
    render_host_result(
        &mut output,
        true,
        "v0.127.0",
        &plan,
        true,
        Some(&evidence),
        None,
    )
    .expect("receipt");
    let receipt: Value = serde_json::from_slice(&output).expect("json");
    for phase in ["binary_pair_before", "binary_pair_after"] {
        assert_eq!(receipt[phase]["primary"]["semantic_version"], "0.127.0");
        assert_eq!(receipt[phase]["companion"]["semantic_version"], "0.127.0");
        assert_eq!(
            receipt[phase]["primary"]["source_identity"],
            receipt[phase]["companion"]["source_identity"]
        );
    }
    assert_eq!(
        receipt["binary_pair_before"]["primary"]["source_identity_basis"],
        "unverified_preinstall"
    );
    assert_eq!(
        receipt["binary_pair_after"]["primary"]["source_identity_basis"],
        "verified_release_authority"
    );
    assert_eq!(receipt["release_authority"]["commit_oid"], "2".repeat(40));
    assert_eq!(receipt["release_authority"]["tree_oid"], "3".repeat(40));
    assert_eq!(
        receipt["release_authority"]["checksum_manifest"]["sha256"],
        "4".repeat(64)
    );
    assert_eq!(
        receipt["release_authority"]["platform_asset"]["attestation_statement_sha256"],
        "7".repeat(64)
    );
}
