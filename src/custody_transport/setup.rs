//! Protected, dry-run-first setup and diagnosis for custody transport.
//!
//! The custody carrier deliberately has no host-enrollment side effect.  This
//! module validates an owner-private policy and the receiver's local SSH
//! contract, then (only with an explicit `--apply`) atomically adds the policy
//! to the machine-global config.  `sshd`, identities, and authorized keys are
//! never modified by Shipyard.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;

use atomicwrites::{AllowOverwrite, AtomicFile};
use rusqlite::{Connection, OpenFlags};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::{DocumentMut, Item};

use super::policy::RawPolicy;
use super::{CustodyTransportPolicy, load_custody_transport_policy};
use crate::identity::RuntimeMode;

mod host;

pub(super) use host::{ReadPrivateError, read_private_input};
use host::{
    derive_public_key_digest, ensure_private_directory, normalize_public_key, read_public_config,
    validate_authorized_keys, validate_private_file, validate_sshd_effective_config,
};
#[cfg(all(test, unix))]
use host::{effective_authorized_keys_path, parse_authorized_key_line, validate_sshd_config};

const SETUP_SCHEMA_VERSION: u32 = 1;
const MAX_SETUP_BYTES: u64 = 256 * 1024;
const MAX_AUTHORIZED_KEYS_BYTES: u64 = 512 * 1024;
const REQUIRED_SUBSYSTEM: &str = "shipyard-custody-v1";

/// One validated custody setup result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CustodySetupReport {
    /// Schema version of this report.
    pub(crate) schema_version: u32,
    /// `disabled`, `verified`, `planned`, `already_configured`, `applied`, or
    /// `refused`.
    pub(crate) outcome: String,
    /// Whether this policy is safe to enable on this host.
    pub(crate) ready: bool,
    /// Canonical digest of the validated policy.
    pub(crate) policy_digest: Option<String>,
    /// Machine identity represented by the policy, if it parsed.
    pub(crate) local_machine_ref: Option<String>,
    /// Validation details with no key or payload material.
    pub(crate) checks: Vec<CustodySetupCheck>,
    /// Exact protected paths inspected or planned for publication.
    pub(crate) paths: Vec<String>,
    /// Stable failure code, when refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reason_code: Option<String>,
}

/// One redacted setup check.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct CustodySetupCheck {
    pub(crate) name: String,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct SetupInput {
    #[serde(default)]
    schema_version: Option<u32>,
    custody_transport: RawPolicy,
}

#[derive(Clone, Debug)]
struct ValidatedSetup {
    raw: RawPolicy,
    policy: CustodyTransportPolicy,
    policy_digest: String,
    checks: Vec<CustodySetupCheck>,
    paths: Vec<String>,
}

/// Read and validate the installed machine-global custody policy.
pub(crate) fn doctor(global_dir: &Path) -> CustodySetupReport {
    let config_path = global_dir.join("config.toml");
    let bytes = match read_private_input(&config_path, MAX_SETUP_BYTES, true) {
        Ok(bytes) => bytes,
        Err(ReadPrivateError::Missing) => {
            return disabled_report("machine-global custody policy is absent");
        }
        Err(error) => return refused_report(error.code(), None, Vec::new()),
    };
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return refused_report("custody-config-malformed", None, Vec::new());
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return refused_report("custody-config-malformed", None, Vec::new());
    };
    let Some(value) = table.get("custody_transport") else {
        return disabled_report("[custody_transport] is absent (carrier remains disabled)");
    };
    let raw: RawPolicy = match value.clone().try_into() {
        Ok(raw) => raw,
        Err(_) => return refused_report("custody-policy-malformed", None, Vec::new()),
    };
    if !raw.enabled {
        return disabled_report("enabled = false (carrier remains disabled)");
    }
    match raw.setup_contract_version {
        None => {
            let digest = policy_digest(&raw).ok();
            return migration_required_report(
                "legacy custody policy requires exact-digest disable and reprovision",
                digest,
            );
        }
        Some(version) if version == super::policy::SETUP_CONTRACT_VERSION => {}
        Some(_) => {
            return refused_report("custody-policy-setup-contract-unknown", None, Vec::new());
        }
    }
    match validate_raw_policy(raw, global_dir) {
        Ok(validated) => report_for("verified", &validated, None),
        Err(failure) => refused_report(&failure.reason_code, failure.policy_digest, failure.checks),
    }
}

/// Validate a private manifest and optionally add its policy atomically to the
/// machine-global config.  Dry-run is represented by `apply = false`.
#[allow(clippy::too_many_lines)]
pub(crate) fn provision(global_dir: &Path, input_path: &Path, apply: bool) -> CustodySetupReport {
    let bytes = match read_private_input(input_path, MAX_SETUP_BYTES, true) {
        Ok(bytes) => bytes,
        Err(error) => return refused_report(error.code(), None, Vec::new()),
    };
    let input: SetupInput = match std::str::from_utf8(&bytes)
        .ok()
        .and_then(|text| toml::from_str(text).ok())
    {
        Some(input) => input,
        None => return refused_report("custody-setup-manifest-malformed", None, Vec::new()),
    };
    if input.schema_version != Some(SETUP_SCHEMA_VERSION) {
        return refused_report("custody-setup-schema-version-refused", None, Vec::new());
    }
    if !input.custody_transport.enabled {
        return disabled_report("manifest enabled = false; no config will be written");
    }
    let raw = input.custody_transport;
    match raw.setup_contract_version {
        Some(version) if version == super::policy::SETUP_CONTRACT_VERSION => {}
        None => return refused_report("custody-setup-contract-marker-missing", None, Vec::new()),
        Some(_) => {
            return refused_report("custody-setup-contract-marker-unknown", None, Vec::new());
        }
    }
    let validated = match validate_raw_policy(raw, global_dir) {
        Ok(validated) => validated,
        Err(failure) => {
            return refused_report(&failure.reason_code, failure.policy_digest, failure.checks);
        }
    };

    let config_path = global_dir.join("config.toml");
    let current = match read_optional_config(&config_path) {
        Ok(current) => current,
        Err(reason) => {
            return refused_report(reason, Some(validated.policy_digest), validated.checks);
        }
    };
    let existing = match current.as_ref() {
        Some(text) => match parse_existing_policy(text) {
            Ok(existing) => existing,
            Err(reason) => {
                return refused_report(reason, Some(validated.policy_digest), validated.checks);
            }
        },
        None => None,
    };
    match existing {
        Some(existing) if existing == validated.raw => {
            return report_for("already_configured", &validated, None);
        }
        Some(_) => {
            return refused_report(
                "custody-policy-existing-different",
                Some(validated.policy_digest),
                validated.checks,
            );
        }
        None => {}
    }
    if !apply {
        return report_for("planned", &validated, None);
    }

    if let Err(reason) = ensure_private_directory(global_dir) {
        return refused_report(reason, Some(validated.policy_digest), validated.checks);
    }
    let _writer_domain = match crate::writer_domain_lease::acquire_for_protected_path(&config_path)
    {
        Ok(lease) => lease,
        Err(error) => {
            return refused_report(
                "custody-config-writer-domain-unavailable",
                Some(validated.policy_digest),
                validated.checks,
            )
            .with_detail(error.to_string());
        }
    };
    // Re-read after acquiring the shared writer lease.  A concurrent writer
    // may have installed an equivalent policy (idempotent success) or a
    // different one (fail closed); neither may be overwritten.
    let current = match read_optional_config(&config_path) {
        Ok(current) => current,
        Err(reason) => {
            return refused_report(reason, Some(validated.policy_digest), validated.checks);
        }
    };
    if let Some(text) = current.as_ref() {
        match parse_existing_policy(text) {
            Ok(Some(existing)) if existing == validated.raw => {
                return report_for("already_configured", &validated, None);
            }
            Ok(Some(_)) => {
                return refused_report(
                    "custody-policy-existing-different",
                    Some(validated.policy_digest),
                    validated.checks,
                );
            }
            Ok(None) => {}
            Err(reason) => {
                return refused_report(reason, Some(validated.policy_digest), validated.checks);
            }
        }
    }
    let mut document = match current {
        Some(text) => match text.parse::<DocumentMut>() {
            Ok(document) => document,
            Err(_) => {
                return refused_report(
                    "custody-config-malformed",
                    Some(validated.policy_digest),
                    validated.checks,
                );
            }
        },
        None => DocumentMut::new(),
    };
    let Some(manifest_table) = toml::to_string(&validated.raw)
        .ok()
        .and_then(|text| text.parse::<DocumentMut>().ok())
    else {
        return refused_report(
            "custody-policy-serialization-failed",
            Some(validated.policy_digest),
            validated.checks,
        );
    };
    let Some(item) = manifest_table.get("enabled").cloned() else {
        return refused_report(
            "custody-policy-serialization-failed",
            Some(validated.policy_digest),
            validated.checks,
        );
    };
    // Assign fields one-by-one through the TOML editor so unrelated global
    // settings remain untouched and the resulting config is parseable.
    let raw_table = manifest_table
        .as_table()
        .iter()
        .map(|(key, value)| (key.to_owned(), value.clone()))
        .collect::<Vec<_>>();
    if !document.contains_key("custody_transport") {
        document["custody_transport"] = Item::Table(toml_edit::Table::new());
    }
    let custody = document
        .get_mut("custody_transport")
        .expect("custody section inserted above");
    let Some(custody) = custody.as_table_mut() else {
        return refused_report(
            "custody-config-section-not-table",
            Some(validated.policy_digest),
            validated.checks,
        );
    };
    for (key, value) in raw_table {
        custody.insert(key.as_str(), value);
    }
    // `item` is deliberately retained in the branch above as a serialization
    // sanity check; all fields are copied from the parsed table, never from
    // string concatenation.
    let _ = item;
    let rendered = document.to_string();
    if let Err(error) = atomic_write_private(&config_path, rendered.as_bytes()) {
        return refused_report(
            "custody-config-write-failed",
            Some(validated.policy_digest),
            validated.checks,
        )
        .with_detail(error.to_string());
    }
    report_for("applied", &validated, None)
}

/// Disable only the exact policy generation named by its digest.
/// External SSH receiver files and any unrelated machine configuration remain
/// untouched.  Dry-run is the default; `apply` performs a leased atomic edit.
#[allow(clippy::too_many_lines)]
pub(crate) fn disable(
    global_dir: &Path,
    state_dir: &Path,
    expected_digest: &str,
    apply: bool,
) -> CustodySetupReport {
    if !is_digest(expected_digest) {
        return refused_report("custody-policy-digest-invalid", None, Vec::new());
    }
    let config_path = global_dir.join("config.toml");
    let current = match read_optional_config(&config_path) {
        Ok(Some(text)) => text,
        Ok(None) => return disabled_report("machine-global custody policy is absent"),
        Err(reason) => return refused_report(reason, None, Vec::new()),
    };
    let existing = match parse_existing_policy(&current) {
        Ok(Some(policy)) => policy,
        Ok(None) => return disabled_report("[custody_transport] is absent"),
        Err(reason) => return refused_report(reason, None, Vec::new()),
    };
    let Ok(actual_digest) = policy_digest(&existing) else {
        return refused_report("custody-policy-serialization-failed", None, Vec::new());
    };
    if actual_digest != expected_digest {
        return refused_report(
            "custody-policy-digest-mismatch",
            Some(actual_digest),
            vec![check(
                "digest",
                false,
                "installed policy differs from requested generation",
            )],
        );
    }
    if let Err(reason) = ensure_no_active_custody_state(state_dir) {
        return refused_report(
            reason,
            Some(actual_digest),
            vec![check(
                "state",
                false,
                "active or indeterminate custody state must drain before disable",
            )],
        );
    }
    let planned = CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "disable_planned".to_owned(),
        ready: true,
        policy_digest: Some(actual_digest.clone()),
        local_machine_ref: existing.local_machine_ref.clone(),
        checks: vec![check("digest", true, "exact installed generation matched")],
        paths: vec![config_path.display().to_string()],
        reason_code: None,
    };
    if !apply {
        return planned;
    }
    if let Err(error) = ensure_private_directory(global_dir) {
        return refused_report(error, Some(actual_digest), planned.checks);
    }
    let _writer_domain = match crate::writer_domain_lease::acquire_for_protected_path(&config_path)
    {
        Ok(lease) => lease,
        Err(error) => {
            return refused_report(
                "custody-config-writer-domain-unavailable",
                Some(actual_digest),
                planned.checks,
            )
            .with_detail(error.to_string());
        }
    };
    // Recheck after acquiring the config writer lease so a concurrent
    // lifecycle transition cannot race the disable publication.
    if let Err(reason) = ensure_no_active_custody_state(state_dir) {
        return refused_report(reason, Some(actual_digest), planned.checks);
    }
    let reread = match read_optional_config(&config_path) {
        Ok(Some(text)) => text,
        Ok(None) => return disabled_report("policy disappeared before disable publication"),
        Err(reason) => return refused_report(reason, Some(actual_digest), planned.checks),
    };
    let reread_policy = match parse_existing_policy(&reread) {
        Ok(Some(policy)) => policy,
        Ok(None) => return disabled_report("policy already absent"),
        Err(reason) => return refused_report(reason, Some(actual_digest), planned.checks),
    };
    let Ok(reread_digest) = policy_digest(&reread_policy) else {
        return refused_report("custody-policy-serialization-failed", None, planned.checks);
    };
    if reread_digest != expected_digest {
        return refused_report(
            "custody-policy-digest-mismatch",
            Some(reread_digest),
            planned.checks,
        );
    }
    let Ok(mut document) = reread.parse::<DocumentMut>() else {
        return refused_report(
            "custody-config-malformed",
            Some(actual_digest),
            planned.checks,
        );
    };
    document.remove("custody_transport");
    let rendered = document.to_string();
    let rendered = if rendered.is_empty() {
        "\n"
    } else {
        rendered.as_str()
    };
    if let Err(error) = atomic_write_private(&config_path, rendered.as_bytes()) {
        return refused_report(
            "custody-config-write-failed",
            Some(actual_digest),
            planned.checks,
        )
        .with_detail(error.to_string());
    }
    let readback = doctor(global_dir);
    if readback.outcome != "disabled" {
        return refused_report(
            "custody-disable-readback-refused",
            Some(actual_digest),
            vec![check(
                "readback",
                false,
                "custody policy remains present after removal",
            )],
        );
    }
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "disabled".to_owned(),
        ready: true,
        policy_digest: Some(actual_digest),
        local_machine_ref: Some(reread_policy.local_machine_ref.unwrap_or_default()),
        checks: vec![check(
            "removed",
            true,
            "exact custody policy removed atomically",
        )],
        paths: vec![config_path.display().to_string()],
        reason_code: None,
    }
}

impl CustodySetupReport {
    fn with_detail(mut self, detail: String) -> Self {
        self.checks.push(CustodySetupCheck {
            name: "write".to_owned(),
            ok: false,
            detail,
        });
        self
    }
}

#[allow(clippy::too_many_lines)]
fn validate_raw_policy(raw: RawPolicy, global_dir: &Path) -> Result<ValidatedSetup, SetupFailure> {
    let policy_digest = policy_digest(&raw)?;
    let temp =
        tempfile::tempdir().map_err(|_| SetupFailure::new("custody-setup-temp-unavailable"))?;
    let config = toml::to_string(&raw)
        .map_err(|_| SetupFailure::new("custody-policy-serialization-failed"))?;
    let config = format!("[custody_transport]\n{config}");
    let mut document = config
        .parse::<DocumentMut>()
        .map_err(|_| SetupFailure::new("custody-policy-serialization-failed"))?;
    if let Some(peers) = document.remove("peers") {
        document["custody_transport"]["peers"] = peers;
    }
    fs::write(temp.path().join("config.toml"), document.to_string())
        .map_err(|_| SetupFailure::new("custody-policy-serialization-failed"))?;
    let policy = load_custody_transport_policy(RuntimeMode::Shipyard, temp.path().to_path_buf())
        .map_err(|reason| SetupFailure::with_digest(reason, policy_digest.clone()))?
        .ok_or_else(|| {
            SetupFailure::with_digest("custody-policy-disabled", policy_digest.clone())
        })?;
    let mut checks = Vec::new();
    let mut paths = vec![global_dir.join("config.toml").display().to_string()];
    checks.push(check("policy", true, "schema and identity bindings valid"));
    for (name, digest) in [
        (
            "destination-bootstrap",
            raw.destination_bootstrap_digest.as_deref(),
        ),
        (
            "native-publication",
            raw.native_publication_digest.as_deref(),
        ),
        ("profile", raw.profile_digest.as_deref()),
    ] {
        let Some(digest) = digest else {
            return Err(SetupFailure::with_all(
                "custody-readiness-receipt-missing",
                Some(policy_digest.clone()),
                checks,
            ));
        };
        if !is_digest(digest) {
            return Err(SetupFailure::with_all(
                "custody-readiness-receipt-invalid",
                Some(policy_digest.clone()),
                checks,
            ));
        }
        checks.push(check(
            name,
            true,
            "owner-attested readiness digest is present",
        ));
    }
    if policy.peers.is_empty() {
        return Err(SetupFailure::with_all(
            "custody-policy-peer-count-invalid",
            Some(policy_digest),
            checks,
        ));
    }
    let mut routes = BTreeSet::new();
    for peer in policy.peers.values() {
        if peer.machine_ref == policy.local_machine_ref
            || peer.incarnation_ref == policy.local_incarnation_ref
            || peer.route_ref == policy.local_route_ref
        {
            return Err(SetupFailure::with_all(
                "custody-policy-local-peer-collision",
                Some(policy_digest),
                checks,
            ));
        }
        if peer.terminal_adapter != policy.local_terminal_adapter {
            return Err(SetupFailure::with_all(
                "custody-policy-terminal-adapter-asymmetric",
                Some(policy_digest),
                checks,
            ));
        }
        if peer.remote_subsystem != REQUIRED_SUBSYSTEM {
            return Err(SetupFailure::with_all(
                "custody-sshd-subsystem-invalid",
                Some(policy_digest),
                checks,
            ));
        }
        if !routes.insert(peer.route_ref.clone()) {
            return Err(SetupFailure::with_all(
                "custody-policy-route-duplicate",
                Some(policy_digest),
                checks,
            ));
        }
        let identity =
            validate_private_file(&peer.identity_file, "identity").map_err(|reason| {
                SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
            })?;
        checks.push(check(
            format!("peer:{}:identity", peer.machine_ref),
            true,
            &identity,
        ));
        paths.push(peer.identity_file.display().to_string());
        let known_hosts =
            validate_private_file(&peer.known_hosts_file, "known-hosts").map_err(|reason| {
                SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
            })?;
        checks.push(check(
            format!("peer:{}:known-hosts", peer.machine_ref),
            true,
            &known_hosts,
        ));
        paths.push(peer.known_hosts_file.display().to_string());
        let inbound_public =
            validate_private_file(&peer.inbound_public_key_file, "inbound-public-key").map_err(
                |reason| {
                    SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
                },
            )?;
        checks.push(check(
            format!("peer:{}:inbound-public-key", peer.machine_ref),
            true,
            &inbound_public,
        ));
        paths.push(peer.inbound_public_key_file.display().to_string());
        let inbound_text = read_public_config(&peer.inbound_public_key_file, MAX_SETUP_BYTES)
            .map_err(|reason| {
                SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
            })?;
        let inbound_key = normalize_public_key(&inbound_text).ok_or_else(|| {
            SetupFailure::with_all(
                "custody-inbound-public-key-invalid",
                Some(policy_digest.clone()),
                checks.clone(),
            )
        })?;
        let inbound_digest = hex::encode(Sha256::digest(inbound_key.as_bytes()));
        if inbound_digest != peer.ssh_auth_key_sha256 {
            return Err(SetupFailure::with_all(
                "custody-peer-inbound-key-hash-mismatch",
                Some(policy_digest.clone()),
                checks,
            ));
        }
        let key_digest = derive_public_key_digest(&peer.identity_file).map_err(|reason| {
            SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
        })?;
        if key_digest != peer.outbound_identity_sha256 {
            return Err(SetupFailure::with_all(
                "custody-peer-outbound-key-hash-mismatch",
                Some(policy_digest),
                checks,
            ));
        }
        checks.push(check(
            format!("peer:{}:public-key", peer.machine_ref),
            true,
            "derived key digest matches policy",
        ));
    }
    let sshd_config = raw.sshd_config_file.as_deref().ok_or_else(|| {
        SetupFailure::with_all(
            "custody-sshd-config-missing",
            Some(policy_digest.clone()),
            checks.clone(),
        )
    })?;
    let sshd_config = super::absolute_path(sshd_config).map_err(|reason| {
        SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
    })?;
    let authorized_keys = raw.authorized_keys_file.as_deref().ok_or_else(|| {
        SetupFailure::with_all(
            "custody-authorized-keys-missing",
            Some(policy_digest.clone()),
            checks.clone(),
        )
    })?;
    let authorized_keys = super::absolute_path(authorized_keys).map_err(|reason| {
        SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
    })?;
    let receiver_program = raw.receiver_program.as_deref().ok_or_else(|| {
        SetupFailure::with_all(
            "custody-receiver-program-missing",
            Some(policy_digest.clone()),
            checks.clone(),
        )
    })?;
    let receiver_program = super::absolute_path(receiver_program).map_err(|reason| {
        SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
    })?;
    // Ask sshd for its effective, include-expanded configuration.  Parsing the
    // one named file is insufficient: fleet hosts keep receiver fragments in
    // sshd_config.d, and an inactive fragment must never be mistaken for the
    // live contract.
    validate_sshd_effective_config(&sshd_config, &authorized_keys, &receiver_program).map_err(
        |reason| SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone()),
    )?;
    checks.push(check(
        "sshd-subsystem",
        true,
        "ExposeAuthInfo and fixed subsystem present",
    ));
    paths.push(sshd_config.display().to_string());

    let authorized_text = validate_private_file(&authorized_keys, "authorized-keys")
        .and_then(|_| read_public_config(&authorized_keys, MAX_AUTHORIZED_KEYS_BYTES))
        .map_err(|reason| {
            SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
        })?;
    validate_authorized_keys(&authorized_text, policy.peers.values()).map_err(|reason| {
        SetupFailure::with_all(reason, Some(policy_digest.clone()), checks.clone())
    })?;
    checks.push(check(
        "authorized-keys",
        true,
        "each configured peer key appears exactly once",
    ));
    paths.push(authorized_keys.display().to_string());
    checks.push(check(
        "receiver-mutation",
        true,
        "Shipyard does not modify sshd, keys, or private routes",
    ));
    let _ = global_dir; // reserved for future path binding; no checkout config is trusted.
    Ok(ValidatedSetup {
        raw,
        policy,
        policy_digest,
        checks,
        paths,
    })
}

fn report_for(
    outcome: &str,
    validated: &ValidatedSetup,
    reason: Option<&str>,
) -> CustodySetupReport {
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: outcome.to_owned(),
        ready: reason.is_none(),
        policy_digest: Some(validated.policy_digest.clone()),
        local_machine_ref: Some(validated.policy.local_machine_ref.clone()),
        checks: validated.checks.clone(),
        paths: validated.paths.clone(),
        reason_code: reason.map(ToOwned::to_owned),
    }
}

fn disabled_report(detail: &str) -> CustodySetupReport {
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "disabled".to_owned(),
        ready: true,
        policy_digest: None,
        local_machine_ref: None,
        checks: vec![check("enabled", true, detail)],
        paths: Vec::new(),
        reason_code: None,
    }
}

fn migration_required_report(detail: &str, policy_digest: Option<String>) -> CustodySetupReport {
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "migration_required".to_owned(),
        ready: false,
        policy_digest,
        local_machine_ref: None,
        checks: vec![check("migration", false, detail)],
        paths: Vec::new(),
        reason_code: Some("custody-policy-migration-required".to_owned()),
    }
}

fn refused_report(
    reason: &str,
    policy_digest: Option<String>,
    checks: Vec<CustodySetupCheck>,
) -> CustodySetupReport {
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "refused".to_owned(),
        ready: false,
        policy_digest,
        local_machine_ref: None,
        checks,
        paths: Vec::new(),
        reason_code: Some(reason.to_owned()),
    }
}

fn check(name: impl Into<String>, ok: bool, detail: impl Into<String>) -> CustodySetupCheck {
    CustodySetupCheck {
        name: name.into(),
        ok,
        detail: detail.into(),
    }
}

fn policy_digest(raw: &RawPolicy) -> Result<String, SetupFailure> {
    let bytes = serde_json::to_vec(raw)
        .map_err(|_| SetupFailure::new("custody-policy-serialization-failed"))?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn read_optional_config(path: &Path) -> Result<Option<String>, &'static str> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err("custody-config-not-regular-file");
            }
            read_private_input(path, MAX_SETUP_BYTES, true)
                .map(|bytes| Some(String::from_utf8_lossy(&bytes).into_owned()))
                .map_err(|error| error.code())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err("custody-config-unavailable"),
    }
}

fn parse_existing_policy(text: &str) -> Result<Option<RawPolicy>, &'static str> {
    let table = text
        .parse::<toml::Table>()
        .map_err(|_| "custody-config-malformed")?;
    table
        .get("custody_transport")
        .map(|value| {
            value
                .clone()
                .try_into()
                .map_err(|_| "custody-policy-malformed")
        })
        .transpose()
}

/// Prove that no in-flight custody operation remains before disabling the
/// carrier. Terminal WAL rows are intentionally retained for audit; this
/// read-only check only rejects active/unknown states and never deletes data.
fn ensure_no_active_custody_state(state_dir: &Path) -> Result<(), &'static str> {
    let db = state_dir.join("work-items.sqlite3");
    if !db.exists() {
        return Ok(());
    }
    let connection = Connection::open_with_flags(
        &db,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|_| "custody-state-unavailable")?;
    let tables = [
        (
            "custody_outbox",
            "state NOT IN ('processed','cancelled','superseded')",
        ),
        (
            "custody_inbox",
            "state NOT IN ('processed','cancelled','superseded')",
        ),
        ("custody_sender_claims", "state = 'active'"),
        ("custody_inbox_claims", "state = 'active'"),
        ("custody_controls", "state = 'pending'"),
        (
            "custody_successor_rebinds",
            "state NOT IN ('finalized','aborted')",
        ),
    ];
    for (table, predicate) in tables {
        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name=?1)",
                [table],
                |row| row.get(0),
            )
            .map_err(|_| "custody-state-unavailable")?;
        if !exists {
            continue;
        }
        let query = format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE {predicate})");
        let active: bool = connection
            .query_row(&query, [], |row| row.get(0))
            .map_err(|_| "custody-state-unavailable")?;
        if active {
            return Err("custody-state-active");
        }
    }
    Ok(())
}

fn atomic_write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    AtomicFile::new(path, AllowOverwrite)
        .write_with_options(
            |file| {
                file.write_all(bytes)?;
                file.sync_all()
            },
            options,
        )
        .map_err(std::io::Error::other)
}

#[derive(Clone, Debug)]
struct SetupFailure {
    reason_code: String,
    policy_digest: Option<String>,
    checks: Vec<CustodySetupCheck>,
}

impl SetupFailure {
    fn new(reason: &str) -> Self {
        Self {
            reason_code: reason.to_owned(),
            policy_digest: None,
            checks: Vec::new(),
        }
    }

    fn with_digest(reason: impl Into<String>, digest: String) -> Self {
        Self {
            reason_code: reason.into(),
            policy_digest: Some(digest),
            checks: Vec::new(),
        }
    }

    fn with_all(
        reason: impl Into<String>,
        digest: Option<String>,
        checks: Vec<CustodySetupCheck>,
    ) -> Self {
        Self {
            reason_code: reason.into(),
            policy_digest: digest,
            checks,
        }
    }
}

#[cfg(all(test, unix))]
#[path = "setup/tests.rs"]
mod tests;
