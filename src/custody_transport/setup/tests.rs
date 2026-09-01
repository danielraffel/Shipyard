use std::os::unix::fs::PermissionsExt;
use std::process::Command;

use super::*;

#[allow(clippy::too_many_lines)]
fn fixture(root: &Path) -> String {
    let identity = root.join("id_ed25519");
    let status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&identity)
        .status()
        .expect("ssh-keygen");
    assert!(status.success());
    fs::set_permissions(&identity, fs::Permissions::from_mode(0o600)).unwrap();
    let key = fs::read_to_string(identity.with_extension("pub")).unwrap();
    let normalized = normalize_public_key(&key).unwrap();
    let key_digest = hex::encode(Sha256::digest(normalized.as_bytes()));
    let inbound_identity = root.join("peer_id_ed25519");
    let inbound_status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&inbound_identity)
        .status()
        .expect("inbound ssh-keygen");
    assert!(inbound_status.success());
    fs::set_permissions(&inbound_identity, fs::Permissions::from_mode(0o600)).unwrap();
    let inbound_key = fs::read_to_string(inbound_identity.with_extension("pub")).unwrap();
    fs::set_permissions(
        inbound_identity.with_extension("pub"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let inbound_normalized = normalize_public_key(&inbound_key).unwrap();
    let inbound_digest = hex::encode(Sha256::digest(inbound_normalized.as_bytes()));

    let known_hosts = root.join("known_hosts");
    fs::write(
        &known_hosts,
        "[peer]:22 ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIfixture\n",
    )
    .unwrap();
    fs::set_permissions(&known_hosts, fs::Permissions::from_mode(0o600)).unwrap();
    let ssh_dir = root.join(".ssh");
    fs::create_dir(&ssh_dir).unwrap();
    fs::set_permissions(&ssh_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let authorized = ssh_dir.join("authorized_keys");
    fs::write(&authorized, inbound_key).unwrap();
    fs::set_permissions(&authorized, fs::Permissions::from_mode(0o600)).unwrap();
    let sshd = root.join("sshd_config");
    let host_key = root.join("host_ed25519");
    let host_status = Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-f"])
        .arg(&host_key)
        .status()
        .expect("host ssh-keygen");
    assert!(host_status.success());
    fs::set_permissions(&host_key, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(
        &sshd,
        format!("HostKey {}\nExposeAuthInfo yes\nAuthorizedKeysFile {}\nSubsystem shipyard-custody-v1 /owner/private/bin/shipyard --mode shipyard work-ledger custody-receive\n", host_key.display(), authorized.display()),
    )
    .unwrap();
    fs::set_permissions(&sshd, fs::Permissions::from_mode(0o600)).unwrap();
    let manifest = format!(
        "schema_version = 1\n\n[custody_transport]\nenabled = true\nlocal_machine_ref = \"machine_{}\"\nlocal_incarnation_ref = \"incarnation_{}\"\nlocal_route_ref = \"route_{}\"\nlocal_terminal_adapter = \"cmux\"\nmutation_authority_machine_ref = \"machine_{}\"\nauthority_digest = \"{}\"\nsender_owner_ref = \"owner_{}\"\ninbox_owner_ref = \"owner_{}\"\nsshd_config_file = \"{}\"\nauthorized_keys_file = \"{}\"\ndestination_bootstrap_digest = \"{}\"\nnative_publication_digest = \"{}\"\nprofile_digest = \"{}\"\nreceiver_program = \"/owner/private/bin/shipyard\"\n\n[[custody_transport.peers]]\nmachine_ref = \"machine_{}\"\nincarnation_ref = \"incarnation_{}\"\nroute_ref = \"route_{}\"\nterminal_adapter = \"cmux\"\nssh_program = \"/usr/bin/ssh\"\ndestination = \"peer\"\nknown_hosts_file = \"{}\"\nidentity_file = \"{}\"\noutbound_identity_sha256 = \"{}\"\ninbound_public_key_file = \"{}\"\nport = 22\nremote_subsystem = \"shipyard-custody-v1\"\nssh_auth_key_sha256 = \"{}\"\n",
        "1".repeat(64),
        "2".repeat(64),
        "3".repeat(64),
        "1".repeat(64),
        "5".repeat(64),
        "6".repeat(64),
        "7".repeat(64),
        sshd.display(),
        authorized.display(),
        "b".repeat(64),
        "c".repeat(64),
        "d".repeat(64),
        "8".repeat(64),
        "9".repeat(64),
        "a".repeat(64),
        known_hosts.display(),
        identity.display(),
        key_digest,
        inbound_identity.with_extension("pub").display(),
        inbound_digest,
    );
    let local_machine = format!("machine_{}", "1".repeat(64));
    let local_incarnation = format!("incarnation_{}", "2".repeat(64));
    let local_route = format!("route_{}", "3".repeat(64));
    let profile_digest = "d".repeat(64);
    let native_digest = "c".repeat(64);
    let bootstrap_digest = "b".repeat(64);
    let authority = "5".repeat(64);
    let mut receipt_paths = Vec::new();
    for (name, kind) in [
        ("bootstrap", "destination_bootstrap"),
        ("publication", "native_publication"),
        ("profile", "profile"),
    ] {
        let canonical = serde_json::json!({"schema_version":1,"kind":kind,"machine_ref":local_machine.clone(),"incarnation_ref":local_incarnation.clone(),"route_ref":local_route.clone(),"authority_digest":authority.clone(),"destination_bootstrap_digest":bootstrap_digest.clone(),"profile_digest":profile_digest.clone(),"native_publication_digest":native_digest.clone()});
        let payload_digest = hex::encode(Sha256::digest(serde_json::to_vec(&canonical).unwrap()));
        let receipt = serde_json::json!({"schema_version":1,"kind":kind,"machine_ref":local_machine.clone(),"incarnation_ref":local_incarnation.clone(),"route_ref":local_route.clone(),"authority_digest":authority.clone(),"destination_bootstrap_digest":bootstrap_digest.clone(),"profile_digest":profile_digest.clone(),"native_publication_digest":native_digest.clone(),"payload_digest":payload_digest});
        let path = root.join(format!("{name}-receipt.json"));
        fs::write(&path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        receipt_paths.push(path);
    }
    manifest
        .replace("enabled = true\n", "enabled = true\nsetup_contract_version = 1\n")
        .replace(
        "receiver_program = \"/owner/private/bin/shipyard\"",
        &format!("receiver_program = \"/owner/private/bin/shipyard\"\ndestination_bootstrap_receipt_file = \"{}\"\nnative_publication_receipt_file = \"{}\"\nprofile_receipt_file = \"{}\"", receipt_paths[0].display(), receipt_paths[1].display(), receipt_paths[2].display()),
        )
}

#[test]
fn doctor_is_disabled_without_machine_policy() {
    let root = tempfile::tempdir().unwrap();
    let report = doctor(root.path());
    assert_eq!(report.outcome, "disabled");
    assert!(report.ready);
    assert!(report.policy_digest.is_none());
}

#[test]
fn legacy_enabled_policy_is_migration_required_but_runtime_compatible() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path());
    let legacy = manifest
        .lines()
        .filter(|line| {
            !line.starts_with("setup_contract_version")
                && !line.starts_with("sshd_config_file")
                && !line.starts_with("authorized_keys_file")
                && !line.starts_with("destination_bootstrap_digest")
                && !line.starts_with("native_publication_digest")
                && !line.starts_with("profile_digest")
                && !line.starts_with("receiver_program")
                && !line.starts_with("destination_bootstrap_receipt_file")
                && !line.starts_with("native_publication_receipt_file")
                && !line.starts_with("profile_receipt_file")
                && !line.starts_with("outbound_identity_sha256")
                && !line.starts_with("inbound_public_key_file")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let global = root.path().join("global");
    fs::create_dir(&global).unwrap();
    let config = global.join("config.toml");
    fs::write(&config, legacy).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let report = doctor(&global);
    assert_eq!(report.outcome, "migration_required");
    assert!(!report.ready);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-policy-migration-required")
    );
    assert!(
        load_custody_transport_policy(RuntimeMode::Shipyard, global)
            .unwrap()
            .is_some()
    );
}

#[test]
fn unknown_setup_contract_marker_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let manifest =
        fixture(root.path()).replace("setup_contract_version = 1", "setup_contract_version = 99");
    let global = root.path().join("global");
    fs::create_dir(&global).unwrap();
    let config = global.join("config.toml");
    fs::write(&config, manifest).unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    let report = doctor(&global);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-policy-setup-contract-unknown")
    );
    assert_eq!(
        load_custody_transport_policy(RuntimeMode::Shipyard, global).unwrap_err(),
        "custody-policy-setup-contract-unknown"
    );
}

#[test]
fn sshd_contract_requires_expose_auth_info_and_fixed_subsystem() {
    assert!(validate_sshd_config(
        "ExposeAuthInfo yes\nSubsystem shipyard-custody-v1 /bin/shipyard --mode shipyard work-ledger custody-receive\n"
    )
    .is_ok());
    assert_eq!(
        validate_sshd_config("ExposeAuthInfo no\n").unwrap_err(),
        "custody-sshd-subsystem-incomplete"
    );
}

#[test]
fn effective_authorized_keys_path_resolves_only_current_home_forms() {
    let home = crate::paths::home_dir();
    let expected = home.join(".ssh/authorized_keys");
    assert_eq!(
        effective_authorized_keys_path(".ssh/authorized_keys").as_deref(),
        Some(expected.as_path())
    );
    assert_eq!(
        effective_authorized_keys_path("%h/.ssh/authorized_keys").as_deref(),
        Some(expected.as_path())
    );
    assert!(effective_authorized_keys_path("%u/.ssh/authorized_keys").is_none());
    assert!(
        effective_authorized_keys_path("/other-user/.ssh/authorized_keys")
            .is_some_and(|path| path != expected)
    );
}

#[test]
fn authorized_key_options_do_not_hide_the_key_identity() {
    assert_eq!(
        parse_authorized_key_line(
            "restrict,command=\"shipyard custody\" ssh-ed25519 AAAATEST comment\n"
        )
        .unwrap()
        .as_deref(),
        Some("ssh-ed25519 AAAATEST")
    );
    assert!(
        parse_authorized_key_line("command=\"ssh-ed25519\"\n")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        parse_authorized_key_line("ssh-ed25519\n").unwrap_err(),
        "custody-authorized-key-malformed"
    );
}

#[test]
fn manifest_apply_is_idempotent_and_conflicts_are_non_destructive() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path());
    let manifest_path = root.path().join("manifest.toml");
    fs::write(&manifest_path, &manifest).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    let global = root.path().join("global");

    let planned = provision(&global, &manifest_path, false);
    assert_eq!(planned.outcome, "planned");
    assert!(planned.ready);
    assert!(!global.exists());

    let applied = provision(&global, &manifest_path, true);
    assert_eq!(applied.outcome, "applied");
    let config_path = global.join("config.toml");
    let before = fs::read(&config_path).unwrap();
    let repeated = provision(&global, &manifest_path, true);
    assert_eq!(repeated.outcome, "already_configured");
    assert_eq!(before, fs::read(&config_path).unwrap());

    let changed = fs::read_to_string(&manifest_path)
        .unwrap()
        .replace(
            "local_route_ref = \"route_3333333333333333333333333333333333333333333333333333333333333333\"",
            "local_route_ref = \"route_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"",
        );
    fs::write(&manifest_path, changed).unwrap();
    let refused = provision(&global, &manifest_path, true);
    assert_eq!(refused.outcome, "refused");
    assert_eq!(
        refused.reason_code.as_deref(),
        Some("custody-policy-readiness-receipt-binding-mismatch")
    );
    assert_eq!(before, fs::read(config_path).unwrap());
}

#[test]
fn manifest_requires_external_readiness_fences() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path()).replace(
        &format!("destination_bootstrap_digest = \"{}\"\n", "b".repeat(64)),
        "",
    );
    let manifest_path = root.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    let report = provision(&root.path().join("global"), &manifest_path, false);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-policy-destination-bootstrap-digest-missing")
    );
}

#[test]
fn provision_requires_explicit_setup_contract_marker() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path()).replace("setup_contract_version = 1\n", "");
    let path = root.path().join("manifest.toml");
    fs::write(&path, manifest).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let report = provision(&root.path().join("global"), &path, false);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-setup-contract-marker-missing")
    );
}

#[test]
fn directional_identity_swap_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path());
    let outbound = root.path().join("id_ed25519.pub");
    let inbound = root.path().join("peer_id_ed25519.pub");
    let outbound_key = normalize_public_key(&fs::read_to_string(&outbound).unwrap()).unwrap();
    let inbound_key = normalize_public_key(&fs::read_to_string(&inbound).unwrap()).unwrap();
    let outbound_digest = hex::encode(Sha256::digest(outbound_key.as_bytes()));
    let inbound_digest = hex::encode(Sha256::digest(inbound_key.as_bytes()));
    assert_ne!(outbound_digest, inbound_digest);
    let swapped = manifest.replace(&outbound_digest, &inbound_digest);
    let path = root.path().join("manifest.toml");
    fs::write(&path, swapped).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let report = provision(&root.path().join("global"), &path, false);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-peer-outbound-key-hash-mismatch")
    );
}

#[test]
fn readiness_receipt_wrong_machine_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path());
    let receipt = root.path().join("bootstrap-receipt.json");
    let tampered = fs::read_to_string(&receipt).unwrap().replace(
        "machine_1111111111111111111111111111111111111111111111111111111111111111",
        "machine_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    fs::write(&receipt, tampered).unwrap();
    let path = root.path().join("manifest.toml");
    fs::write(&path, manifest).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let report = provision(&root.path().join("global"), &path, false);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-policy-readiness-receipt-binding-mismatch")
    );
}

#[test]
fn readiness_receipt_wrong_kind_and_payload_are_refused() {
    for (needle, replacement, expected) in [
        (
            "\"kind\":\"destination_bootstrap\"",
            "\"kind\":\"profile\"",
            "custody-policy-readiness-receipt-binding-mismatch",
        ),
        (
            "\"payload_digest\":\"",
            "\"payload_digest\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"_bad\":\"",
            "custody-policy-readiness-receipt-malformed",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let receipt = root.path().join("bootstrap-receipt.json");
        let original = fs::read_to_string(&receipt).unwrap();
        fs::write(&receipt, original.replace(needle, replacement)).unwrap();
        let path = root.path().join("manifest.toml");
        fs::write(&path, manifest).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let report = provision(&root.path().join("global"), &path, false);
        assert_eq!(report.reason_code.as_deref(), Some(expected));
    }
}

#[test]
fn readiness_receipt_bootstrap_binding_is_required_and_exact() {
    for (mutate, expected) in [
        ("wrong", "custody-policy-readiness-receipt-binding-mismatch"),
        ("missing", "custody-policy-readiness-receipt-malformed"),
    ] {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let receipt = root.path().join("bootstrap-receipt.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        if mutate == "wrong" {
            json["destination_bootstrap_digest"] = serde_json::Value::String("e".repeat(64));
        } else {
            json.as_object_mut()
                .unwrap()
                .remove("destination_bootstrap_digest");
        }
        fs::write(&receipt, serde_json::to_vec(&json).unwrap()).unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let path = root.path().join("manifest.toml");
        fs::write(&path, manifest).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let report = provision(&root.path().join("global"), &path, false);
        assert_eq!(report.reason_code.as_deref(), Some(expected), "{mutate}");
    }
}

#[test]
fn readiness_receipt_bootstrap_replay_is_refused_after_policy_change() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path()).replace(
        &format!("destination_bootstrap_digest = \"{}\"", "b".repeat(64)),
        &format!("destination_bootstrap_digest = \"{}\"", "e".repeat(64)),
    );
    let path = root.path().join("manifest.toml");
    fs::write(&path, manifest).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let report = provision(&root.path().join("global"), &path, false);
    assert_eq!(
        report.reason_code.as_deref(),
        Some("custody-policy-readiness-receipt-binding-mismatch")
    );
}

#[test]
fn readiness_receipt_wrong_incarnation_route_authority_and_payload_are_refused() {
    for (field, value, expected) in [
        (
            "incarnation_ref",
            "incarnation_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .to_owned(),
            "custody-policy-readiness-receipt-binding-mismatch",
        ),
        (
            "route_ref",
            "route_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            "custody-policy-readiness-receipt-binding-mismatch",
        ),
        (
            "authority_digest",
            "e".repeat(64),
            "custody-policy-readiness-receipt-binding-mismatch",
        ),
        (
            "payload_digest",
            "0".repeat(64),
            "custody-policy-readiness-receipt-digest-mismatch",
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let receipt = root.path().join("bootstrap-receipt.json");
        let mut json: serde_json::Value =
            serde_json::from_slice(&fs::read(&receipt).unwrap()).unwrap();
        json[field] = serde_json::Value::String(value);
        fs::write(&receipt, serde_json::to_vec(&json).unwrap()).unwrap();
        fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
        let path = root.path().join("manifest.toml");
        fs::write(&path, manifest).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let report = provision(&root.path().join("global"), &path, false);
        assert_eq!(report.reason_code.as_deref(), Some(expected), "{field}");
    }
}

#[test]
fn readiness_receipt_file_safety_is_fail_closed() {
    for mode in ["mode", "symlink", "oversize"] {
        let root = tempfile::tempdir().unwrap();
        let manifest = fixture(root.path());
        let receipt = root.path().join("bootstrap-receipt.json");
        match mode {
            "mode" => fs::set_permissions(&receipt, fs::Permissions::from_mode(0o644)).unwrap(),
            "symlink" => {
                let target = root.path().join("real-receipt.json");
                fs::rename(&receipt, &target).unwrap();
                std::os::unix::fs::symlink(&target, &receipt).unwrap();
            }
            _ => {
                let mut bytes = fs::read(&receipt).unwrap();
                bytes.resize(65 * 1024, b'x');
                fs::write(&receipt, bytes).unwrap();
                fs::set_permissions(&receipt, fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        let path = root.path().join("manifest.toml");
        fs::write(&path, manifest).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let report = provision(&root.path().join("global"), &path, false);
        assert!(!report.ready, "{mode} receipt must refuse");
    }
}

#[test]
fn disable_requires_exact_digest_and_preserves_unrelated_config() {
    let root = tempfile::tempdir().unwrap();
    let manifest = fixture(root.path());
    let manifest_path = root.path().join("manifest.toml");
    fs::write(&manifest_path, manifest).unwrap();
    fs::set_permissions(&manifest_path, fs::Permissions::from_mode(0o600)).unwrap();
    let global = root.path().join("global");
    assert_eq!(provision(&global, &manifest_path, true).outcome, "applied");
    let config_path = global.join("config.toml");
    let installed = fs::read_to_string(&config_path).unwrap();
    let raw = parse_existing_policy(&installed).unwrap().unwrap();
    let digest = policy_digest(&raw).unwrap();
    let with_other = format!("[other]\nvalue = 7\n\n{installed}");
    fs::write(&config_path, with_other).unwrap();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        disable(&global, &root.path().join("state"), &"e".repeat(64), true).outcome,
        "refused"
    );
    assert!(
        fs::read_to_string(&config_path)
            .unwrap()
            .contains("[custody_transport]")
    );
    assert_eq!(
        disable(&global, &root.path().join("state"), &digest, false).outcome,
        "disable_planned"
    );
    assert_eq!(
        disable(&global, &root.path().join("state"), &digest, true).outcome,
        "disabled"
    );
    let remaining = fs::read_to_string(config_path).unwrap();
    assert!(remaining.contains("[other]"));
    assert!(!remaining.contains("[custody_transport]"));
    assert_eq!(doctor(&global).outcome, "disabled");
}
