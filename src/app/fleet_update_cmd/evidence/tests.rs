#[cfg(unix)]
use std::process::Stdio;

use super::*;
use crate::capacity::HostClassConfig;

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
    }
}

#[test]
fn remote_evidence_is_typed_and_proves_repo_preservation() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.127.0").expect("plan");
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
        "shipyard_version": "0.127.0"
    });
    let stdout = format!(
        "{REMOTE_BEFORE_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_BEFORE_PRIMARY_VERSION_PREFIX}shipyard 0.126.2\n{REMOTE_BEFORE_COMPANION_SHA256_PREFIX}absent\n{REMOTE_BEFORE_COMPANION_VERSION_PREFIX}absent\n{REMOTE_AFTER_PRIMARY_SHA256_PREFIX}{}\n{REMOTE_AFTER_PRIMARY_VERSION_PREFIX}shipyard 0.127.0\n{REMOTE_AFTER_COMPANION_SHA256_PREFIX}{}\n{REMOTE_AFTER_COMPANION_VERSION_PREFIX}shipyard-workstream-provider 0.127.0\n{}{}\n{}755\n{}{}\n{}755\n{}{}\n{}700\n{}{}\n{}700\n{REMOTE_BEFORE_STATUS_PREFIX}{before}\n{REMOTE_REFRESH_PREFIX}{refresh}\n{REMOTE_AFTER_STATUS_PREFIX}{after}\n{REMOTE_AUTHORITY_ID_PREFIX}{}\n{REMOTE_RELEASE_ASSET_SHA256_PREFIX}{}\n",
        "a".repeat(64),
        "b".repeat(64),
        "c".repeat(64),
        auth_support::BEFORE_HELPER_SHA_PREFIX,
        "f".repeat(64),
        auth_support::BEFORE_HELPER_MODE_PREFIX,
        auth_support::BEFORE_WRAPPER_SHA_PREFIX,
        "a".repeat(64),
        auth_support::BEFORE_WRAPPER_MODE_PREFIX,
        auth_support::AFTER_HELPER_SHA_PREFIX,
        "c".repeat(64),
        auth_support::AFTER_HELPER_MODE_PREFIX,
        auth_support::AFTER_WRAPPER_SHA_PREFIX,
        "e".repeat(64),
        auth_support::AFTER_WRAPPER_MODE_PREFIX,
        "8".repeat(64),
        "6".repeat(64),
    );
    let evidence = parse_remote_evidence(&plan, stdout.as_bytes()).expect("evidence");
    assert_eq!(evidence.daemon_pid, 4242);
    assert_eq!(evidence.configured_repos_preserved, Some(true));
    validate_evidence(&plan, &evidence).expect("valid evidence");
}

#[test]
fn evidence_rejects_version_repo_and_digest_drift() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.127.0").expect("plan");
    let mut observed = evidence("0.127.0");
    observed.daemon_version = "0.126.2".to_owned();
    assert!(validate_evidence(&plan, &observed).is_err());
    observed.daemon_version = "0.127.0".to_owned();
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
fn auth_support_evidence_rejects_tamper_and_mixed_release_source() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.127.0").expect("plan");
    let mut observed = evidence("0.127.0");
    observed.auth_support_after.helper.sha256 = Some("f".repeat(64));
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed.auth_support_after.wrapper.source_blob_oid = Some("b".repeat(40));
    assert!(validate_evidence(&plan, &observed).is_err());

    observed.auth_support_after = auth_support(true);
    observed.auth_support_after.wrapper.mode = Some(0o755);
    assert!(validate_evidence(&plan, &observed).is_err());
}

#[test]
fn evidence_never_accepts_mixed_pair_or_legacy_companion() {
    let paired_plan =
        super::super::host_update_plan(&host(Some("m5-lan")), "v0.127.0").expect("paired plan");
    let mut mixed = evidence("0.127.0");
    mixed
        .after_pair
        .companion
        .as_mut()
        .expect("companion")
        .semantic_version = "0.126.4".to_owned();
    assert!(validate_evidence(&paired_plan, &mixed).is_err());

    let mut wrong_source = evidence("0.127.0");
    wrong_source.after_pair.primary.source_identity = Some("9".repeat(64));
    assert!(validate_evidence(&paired_plan, &wrong_source).is_err());

    let legacy_plan =
        super::super::host_update_plan(&host(Some("m5-lan")), "v0.126.2").expect("legacy plan");
    let mut legacy_mixed = evidence("0.126.2");
    legacy_mixed.after_pair.companion = Some(BinaryEvidence {
        path: legacy_plan.companion_binary.clone(),
        semantic_version: "0.126.2".to_owned(),
        sha256: "d".repeat(64),
        source_identity: Some(verified_source_identity()),
        source_identity_basis: SourceIdentityBasis::VerifiedReleaseAuthority,
    });
    assert!(validate_evidence(&legacy_plan, &legacy_mixed).is_err());
}

#[test]
fn evidence_never_infers_preinstall_provenance_or_omits_postinstall_binding() {
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.127.0").expect("plan");

    let mut fabricated_before = evidence("0.127.0");
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

    let mut unbound_after = evidence("0.127.0");
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
        "shipyard_version": "0.127.0"
    });
    let pair = evidence("0.127.0").after_pair;
    let observed = evidence_from_values(
        pair.clone(),
        pair,
        auth_support(false),
        auth_support(true),
        9,
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
    let plan = super::super::host_update_plan(&host(Some("m5-lan")), "v0.127.0").expect("plan");
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
        "#!/bin/sh\ncase \"$*\" in *\"daemon status\"*) printf '%s\\n' '{\"command\":\"daemon:status\",\"running\":false}' ;; *\"--version\"*) sleep 60 ;; *) printf '%s\\n' '{\"event\":\"daemon_refreshed\",\"daemon_pid\":42}' ;; esac\n",
    )
    .expect("fixture");
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).expect("executable");
    let mut class = host(None);
    class.shipyard_bin = Some(binary.display().to_string());
    let plan = super::super::host_update_plan(&class, "v0.100.0").expect("plan");
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
