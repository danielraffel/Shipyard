//! Crash-safe exact-generation custody policy disablement.
//!
//! The machine policy and custody ledger are separate protected surfaces.  A
//! durable intent therefore precedes the config replacement, while an
//! immutable completion receipt follows exact config and history readback.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use rusqlite::types::ValueRef;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use toml_edit::DocumentMut;

use super::{
    CustodySetupReport, RawPolicy, SETUP_SCHEMA_VERSION, atomic_write_private, check,
    disabled_report, ensure_private_directory, is_digest, parse_existing_policy, policy_digest,
    read_optional_config, refused_report,
};
use crate::immutable_store::ImmutableByteStore;
use crate::parallel_proof::StoreWriteOutcome;
use crate::work_ledger::{WorkLedger, WorkLedgerError};

const RECORD_SCHEMA_VERSION: u32 = 1;
const MAX_RECORD_BYTES: usize = 32 * 1024;
const RECORD_STORE_NAME: &str = "custody-disable-records";
const INTENT_DOMAIN: &[u8] = b"shipyard.custody-disable.intent.v1\0";
const RECEIPT_DOMAIN: &[u8] = b"shipyard.custody-disable.receipt.v1\0";
const HISTORY_DOMAIN: &[u8] = b"shipyard.custody-disable.history.v1\0";

const CUSTODY_TABLES: &[&str] = &[
    "custody_controls",
    "custody_effects",
    "custody_events",
    "custody_inbox",
    "custody_inbox_claims",
    "custody_outbox",
    "custody_processed_acknowledgements",
    "custody_rebinds",
    "custody_sender_claims",
    "custody_successor_events",
    "custody_successor_rebinds",
];

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct ConfigFence {
    exists: bool,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CustodyHistoryFence {
    ledger_exists: bool,
    schema_version: i64,
    schema_digest: String,
    rows: u64,
    digest: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DisableIntentPayload {
    schema_version: u32,
    sequence: u64,
    predecessor_receipt_digest: Option<String>,
    policy_digest: String,
    local_machine_ref: Option<String>,
    local_incarnation_ref: Option<String>,
    local_route_ref: Option<String>,
    authority_digest: Option<String>,
    before_config: ConfigFence,
    after_config: ConfigFence,
    custody_history: CustodyHistoryFence,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DisableReceiptPayload {
    schema_version: u32,
    sequence: u64,
    intent_digest: String,
    policy_digest: String,
    after_config: ConfigFence,
    custody_history: CustodyHistoryFence,
    outcome: String,
}

#[derive(Default)]
struct DisableRecordIndex {
    intents: BTreeMap<String, DisableIntentPayload>,
    receipts: BTreeMap<String, DisableRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "record", rename_all = "snake_case", deny_unknown_fields)]
enum DisableRecord {
    Intent {
        intent_digest: String,
        payload: DisableIntentPayload,
    },
    Receipt {
        receipt_digest: String,
        payload: DisableReceiptPayload,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DisableCheckpoint {
    None,
    AfterIntent,
    AfterConfig,
    AfterReceipt,
}

/// Disable only the exact policy generation named by its digest.
///
/// Dry-run remains no-write. Apply holds the machine-wide exclusive writer
/// domain through durable intent, config publication, history readback, and
/// immutable receipt publication.
pub(crate) fn disable(
    global_dir: &Path,
    state_dir: &Path,
    expected_digest: &str,
    apply: bool,
) -> CustodySetupReport {
    disable_with_checkpoint(
        global_dir,
        state_dir,
        expected_digest,
        apply,
        DisableCheckpoint::None,
    )
}

#[allow(clippy::too_many_lines)]
fn disable_with_checkpoint(
    global_dir: &Path,
    state_dir: &Path,
    expected_digest: &str,
    apply: bool,
    checkpoint: DisableCheckpoint,
) -> CustodySetupReport {
    if !is_digest(expected_digest) {
        return refused_report("custody-policy-digest-invalid", None, Vec::new());
    }
    let read_snapshot = if apply {
        None
    } else {
        match crate::writer_domain_lease::acquire_existing_exclusive_for_protected_path(state_dir) {
            Ok(lease) => lease,
            Err(error) => {
                return refused_report(
                    "custody-state-read-barrier-unavailable",
                    Some(expected_digest.to_owned()),
                    Vec::new(),
                )
                .with_detail(error.to_string());
            }
        }
    };
    let config_path = global_dir.join("config.toml");
    let initial = match read_optional_config(&config_path) {
        Ok(value) => value,
        Err(reason) => return refused_report(reason, None, Vec::new()),
    };
    let initial_policy = match initial.as_deref().map(parse_existing_policy).transpose() {
        Ok(value) => value.flatten(),
        Err(reason) => return refused_report(reason, None, Vec::new()),
    };
    if let Some(policy) = initial_policy.as_ref() {
        let Ok(actual_digest) = policy_digest(policy) else {
            return refused_report("custody-policy-serialization-failed", None, Vec::new());
        };
        if actual_digest != expected_digest {
            return digest_mismatch(actual_digest);
        }
        if !apply {
            if let Err(reason) = custody_history_fence(state_dir, read_snapshot.as_ref()) {
                return state_refusal(reason, Some(actual_digest));
            }
            let Some(current) = initial.as_deref() else {
                return refused_report(
                    "custody-config-generation-ambiguous",
                    Some(actual_digest),
                    Vec::new(),
                );
            };
            return plan_enabled_disable(
                &config_path,
                state_dir,
                read_snapshot.as_ref(),
                current,
                policy,
                actual_digest,
            );
        }
    } else if !apply {
        return default_off_report(
            &config_path,
            state_dir,
            read_snapshot.as_ref(),
            initial.as_deref(),
            expected_digest,
        );
    }

    if initial_policy.is_some()
        && let Err(error) = ensure_private_directory(global_dir)
    {
        return refused_report(error, Some(expected_digest.to_owned()), Vec::new());
    }
    let exclusive_writer_domain =
        match crate::writer_domain_lease::acquire_exclusive_for_protected_path(state_dir) {
            Ok(lease) => lease,
            Err(error) => {
                return refused_report(
                    "custody-state-writer-domain-unavailable",
                    Some(expected_digest.to_owned()),
                    Vec::new(),
                )
                .with_detail(error.to_string());
            }
        };
    let _config_writer = match crate::writer_domain_lease::acquire_for_protected_path(&config_path)
    {
        Ok(lease) => lease,
        Err(error) => {
            return refused_report(
                "custody-config-writer-domain-unavailable",
                Some(expected_digest.to_owned()),
                Vec::new(),
            )
            .with_detail(error.to_string());
        }
    };
    disable_locked(
        global_dir,
        state_dir,
        &config_path,
        exclusive_writer_domain.as_ref(),
        expected_digest,
        checkpoint,
    )
}

#[allow(clippy::too_many_lines)] // One lease-held state machine keeps intent, config, and receipt ordering auditable.
fn disable_locked(
    global_dir: &Path,
    state_dir: &Path,
    config_path: &Path,
    snapshot_barrier: Option<&crate::writer_domain_lease::ProductionSnapshotLease>,
    expected_digest: &str,
    checkpoint: DisableCheckpoint,
) -> CustodySetupReport {
    let current = match read_optional_config(config_path) {
        Ok(value) => value,
        Err(reason) => return refused_report(reason, Some(expected_digest.to_owned()), Vec::new()),
    };
    let history = match custody_history_fence(state_dir, snapshot_barrier) {
        Ok(fence) => fence,
        Err(reason) => return state_refusal(reason, Some(expected_digest.to_owned())),
    };
    let current_policy = match current.as_deref().map(parse_existing_policy).transpose() {
        Ok(value) => value.flatten(),
        Err(reason) => {
            return refused_report(reason, Some(expected_digest.to_owned()), Vec::new());
        }
    };
    let store = match current_policy.as_ref() {
        Some(_) => match open_store(state_dir) {
            Ok(store) => store,
            Err(reason) => return store_refusal(reason, expected_digest),
        },
        None => match open_store_read_only(state_dir) {
            Ok(Some(store)) => store,
            Ok(None) => {
                return disabled_report("custody policy is absent; no matching disable receipt");
            }
            Err(reason) => return store_refusal(reason, expected_digest),
        },
    };
    let records = match load_records(&store) {
        Ok(records) => records,
        Err(reason) => return store_refusal(reason, expected_digest),
    };

    let intent = if let Some(policy) = current_policy {
        let Ok(actual_digest) = policy_digest(&policy) else {
            return refused_report("custody-policy-serialization-failed", None, Vec::new());
        };
        if actual_digest != expected_digest {
            return digest_mismatch(actual_digest);
        }
        let Some(before_text) = current.as_deref() else {
            return refused_report(
                "custody-config-generation-ambiguous",
                Some(actual_digest),
                Vec::new(),
            );
        };
        let after_text = match render_without_policy(before_text) {
            Ok(text) => text,
            Err(reason) => return refused_report(reason, Some(actual_digest), Vec::new()),
        };
        let before_config = config_fence(Some(before_text));
        let after_config = config_fence(Some(&after_text));
        let payload = match recover_enabled_intent(
            &records,
            expected_digest,
            &before_config,
            &after_config,
            &history,
        ) {
            Ok(Some(intent)) => intent,
            Ok(None) => {
                let (sequence, predecessor_receipt_digest) = match next_intent_generation(&records)
                {
                    Ok(generation) => generation,
                    Err(reason) => return store_refusal(reason, expected_digest),
                };
                let payload = DisableIntentPayload {
                    schema_version: RECORD_SCHEMA_VERSION,
                    sequence,
                    predecessor_receipt_digest,
                    policy_digest: actual_digest,
                    local_machine_ref: policy.local_machine_ref,
                    local_incarnation_ref: policy.local_incarnation_ref,
                    local_route_ref: policy.local_route_ref,
                    authority_digest: policy.authority_digest,
                    before_config,
                    after_config,
                    custody_history: history.clone(),
                };
                let intent_record = intent_record(payload.clone());
                if let Err(reason) = put_record(&store, &intent_record) {
                    return store_refusal(reason, expected_digest);
                }
                payload
            }
            Err(reason) => return store_refusal(reason, expected_digest),
        };
        if checkpoint == DisableCheckpoint::AfterIntent {
            return interrupted_report(expected_digest, "after_intent");
        }
        if let Err(error) = write_config_durable(config_path, after_text.as_bytes()) {
            return refused_report(
                "custody-config-write-failed",
                Some(expected_digest.to_owned()),
                Vec::new(),
            )
            .with_detail(error.to_string());
        }
        if checkpoint == DisableCheckpoint::AfterConfig {
            return interrupted_report(expected_digest, "after_config");
        }
        payload
    } else {
        match recover_disabled_intent(
            &records,
            expected_digest,
            &config_fence(current.as_deref()),
            &history,
        ) {
            Ok(Some((intent, Some(receipt)))) => {
                return completed_report(config_path, &intent, &receipt, true);
            }
            Ok(Some((intent, None))) => intent,
            Ok(None) => {
                return disabled_report("custody policy is absent; no matching disable receipt");
            }
            Err(reason) => return store_refusal(reason, expected_digest),
        }
    };

    let current_after = match read_optional_config(config_path) {
        Ok(value) => value,
        Err(reason) => return refused_report(reason, Some(expected_digest.to_owned()), Vec::new()),
    };
    let current_after_policy = match current_after
        .as_deref()
        .map(parse_existing_policy)
        .transpose()
    {
        Ok(value) => value.flatten(),
        Err(reason) => return refused_report(reason, Some(expected_digest.to_owned()), Vec::new()),
    };
    if current_after_policy.is_some()
        || config_fence(current_after.as_deref()) != intent.after_config
    {
        return refused_report(
            "custody-disable-config-readback-mismatch",
            Some(expected_digest.to_owned()),
            Vec::new(),
        );
    }
    let after_history = match custody_history_fence(state_dir, snapshot_barrier) {
        Ok(fence) => fence,
        Err(reason) => return state_refusal(reason, Some(expected_digest.to_owned())),
    };
    if after_history != intent.custody_history {
        return refused_report(
            "custody-history-changed-during-disable",
            Some(expected_digest.to_owned()),
            Vec::new(),
        );
    }
    if super::doctor(global_dir).outcome != "disabled" {
        return refused_report(
            "custody-disable-readback-refused",
            Some(expected_digest.to_owned()),
            Vec::new(),
        );
    }
    let receipt = receipt_record(&intent);
    if let Err(reason) = put_record(&store, &receipt) {
        return store_refusal(reason, expected_digest);
    }
    if checkpoint == DisableCheckpoint::AfterReceipt {
        return interrupted_report(expected_digest, "after_receipt");
    }
    completed_report(config_path, &intent, &receipt, false)
}

fn default_off_report(
    config_path: &Path,
    state_dir: &Path,
    snapshot_barrier: Option<&crate::writer_domain_lease::ProductionSnapshotLease>,
    current: Option<&str>,
    expected_digest: &str,
) -> CustodySetupReport {
    let history = match custody_history_fence(state_dir, snapshot_barrier) {
        Ok(fence) => fence,
        Err(reason) => return state_refusal(reason, Some(expected_digest.to_owned())),
    };
    let store = match open_store_read_only(state_dir) {
        Ok(Some(store)) => store,
        Ok(None) => return disabled_report("custody policy is absent"),
        Err(reason) => return store_refusal(reason, expected_digest),
    };
    let records = match load_records(&store) {
        Ok(records) => records,
        Err(reason) => return store_refusal(reason, expected_digest),
    };
    match recover_disabled_intent(&records, expected_digest, &config_fence(current), &history) {
        Ok(Some((intent, Some(receipt)))) => completed_report(config_path, &intent, &receipt, true),
        Ok(Some((intent, None))) => CustodySetupReport {
            schema_version: SETUP_SCHEMA_VERSION,
            outcome: "disable_recovery_planned".to_owned(),
            ready: true,
            policy_digest: Some(expected_digest.to_owned()),
            local_machine_ref: intent.local_machine_ref,
            checks: vec![check(
                "receipt",
                false,
                "durable disable intent awaits completion receipt",
            )],
            paths: vec![record_store_path(state_dir).display().to_string()],
            reason_code: None,
            receipt_digest: None,
        },
        Ok(None) => disabled_report("custody policy is absent; no matching disable receipt"),
        Err(reason) => store_refusal(reason, expected_digest),
    }
}

fn planned_report(config_path: &Path, policy: &RawPolicy, digest: String) -> CustodySetupReport {
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "disable_planned".to_owned(),
        ready: true,
        policy_digest: Some(digest),
        local_machine_ref: policy.local_machine_ref.clone(),
        checks: vec![check("digest", true, "exact installed generation matched")],
        paths: vec![config_path.display().to_string()],
        reason_code: None,
        receipt_digest: None,
    }
}

fn plan_enabled_disable(
    config_path: &Path,
    state_dir: &Path,
    snapshot_barrier: Option<&crate::writer_domain_lease::ProductionSnapshotLease>,
    current: &str,
    policy: &RawPolicy,
    digest: String,
) -> CustodySetupReport {
    let store = match open_store_read_only(state_dir) {
        Ok(Some(store)) => store,
        Ok(None) => return planned_report(config_path, policy, digest),
        Err(reason) => return store_refusal(reason, &digest),
    };
    let records = match load_records(&store) {
        Ok(records) => records,
        Err(reason) => return store_refusal(reason, &digest),
    };
    let history = match custody_history_fence(state_dir, snapshot_barrier) {
        Ok(history) => history,
        Err(reason) => return state_refusal(reason, Some(digest)),
    };
    let after = match render_without_policy(current) {
        Ok(after) => after,
        Err(reason) => return refused_report(reason, Some(digest), Vec::new()),
    };
    match recover_enabled_intent(
        &records,
        &digest,
        &config_fence(Some(current)),
        &config_fence(Some(&after)),
        &history,
    ) {
        Ok(Some(intent)) => CustodySetupReport {
            schema_version: SETUP_SCHEMA_VERSION,
            outcome: "disable_recovery_planned".to_owned(),
            ready: true,
            policy_digest: Some(digest),
            local_machine_ref: intent.local_machine_ref,
            checks: vec![check(
                "receipt",
                false,
                "durable disable intent awaits config publication and completion receipt",
            )],
            paths: vec![record_store_path(state_dir).display().to_string()],
            reason_code: None,
            receipt_digest: None,
        },
        Ok(None) => planned_report(config_path, policy, digest),
        Err(reason) => store_refusal(reason, &digest),
    }
}

fn completed_report(
    config_path: &Path,
    intent: &DisableIntentPayload,
    receipt: &DisableRecord,
    replayed: bool,
) -> CustodySetupReport {
    let DisableRecord::Receipt { receipt_digest, .. } = receipt else {
        unreachable!("completion requires a receipt record")
    };
    CustodySetupReport {
        schema_version: SETUP_SCHEMA_VERSION,
        outcome: "disabled".to_owned(),
        ready: true,
        policy_digest: Some(intent.policy_digest.clone()),
        local_machine_ref: intent.local_machine_ref.clone(),
        checks: vec![
            check(
                "history",
                true,
                "exact custody ledger snapshot is unchanged",
            ),
            check(
                "receipt",
                true,
                if replayed {
                    "exact immutable disable receipt replayed"
                } else {
                    "exact immutable disable receipt committed"
                },
            ),
        ],
        paths: vec![config_path.display().to_string()],
        reason_code: None,
        receipt_digest: Some(receipt_digest.clone()),
    }
}

fn digest_mismatch(actual_digest: String) -> CustodySetupReport {
    refused_report(
        "custody-policy-digest-mismatch",
        Some(actual_digest),
        vec![check(
            "digest",
            false,
            "installed policy differs from requested generation",
        )],
    )
}

fn state_refusal(reason: &'static str, digest: Option<String>) -> CustodySetupReport {
    refused_report(
        reason,
        digest,
        vec![check(
            "state",
            false,
            "active or indeterminate custody state must drain before disable",
        )],
    )
}

fn store_refusal(reason: String, digest: &str) -> CustodySetupReport {
    refused_report(
        "custody-disable-receipt-unavailable",
        Some(digest.to_owned()),
        Vec::new(),
    )
    .with_detail(reason)
}

fn interrupted_report(digest: &str, checkpoint: &str) -> CustodySetupReport {
    refused_report(
        "custody-disable-interrupted",
        Some(digest.to_owned()),
        vec![check("checkpoint", false, checkpoint)],
    )
}

fn render_without_policy(text: &str) -> Result<String, &'static str> {
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|_| "custody-config-malformed")?;
    document.remove("custody_transport");
    let rendered = document.to_string();
    Ok(if rendered.is_empty() {
        "\n".to_owned()
    } else {
        rendered
    })
}

fn write_config_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write_private(path, bytes)?;
    crate::log_retention::sync_parent_directory(path)
}

fn record_store_path(state_dir: &Path) -> PathBuf {
    state_dir.join(RECORD_STORE_NAME)
}

fn open_store(state_dir: &Path) -> Result<ImmutableByteStore, String> {
    crate::writer_domain_lease::ensure_protected_dir_all(state_dir)
        .map_err(|error| error.to_string())?;
    ImmutableByteStore::open(record_store_path(state_dir), MAX_RECORD_BYTES)
        .map_err(|error| error.to_string())
}

fn open_store_read_only(state_dir: &Path) -> Result<Option<ImmutableByteStore>, String> {
    let root = record_store_path(state_dir);
    match fs::symlink_metadata(&root) {
        Ok(_) => ImmutableByteStore::open_read_only(root, MAX_RECORD_BYTES)
            .map(Some)
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn disable_record_store_exists(state_dir: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(record_store_path(state_dir)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

pub(super) fn provisioning_fence(state_dir: &Path) -> Result<(), String> {
    let Some(store) = open_store_read_only(state_dir)? else {
        return Ok(());
    };
    let records = load_records(&store)?;
    if pending_intent(&records)?.is_some() {
        return Err("an unresolved custody disable generation must complete first".to_owned());
    }
    Ok(())
}

fn intent_record(payload: DisableIntentPayload) -> DisableRecord {
    let intent_digest = canonical_digest(INTENT_DOMAIN, &payload).expect("serializable intent");
    DisableRecord::Intent {
        intent_digest,
        payload,
    }
}

fn receipt_record(intent: &DisableIntentPayload) -> DisableRecord {
    let intent_digest = canonical_digest(INTENT_DOMAIN, intent).expect("serializable intent");
    let payload = DisableReceiptPayload {
        schema_version: RECORD_SCHEMA_VERSION,
        sequence: intent.sequence,
        intent_digest,
        policy_digest: intent.policy_digest.clone(),
        after_config: intent.after_config.clone(),
        custody_history: intent.custody_history.clone(),
        outcome: "disabled".to_owned(),
    };
    let receipt_digest = canonical_digest(RECEIPT_DOMAIN, &payload).expect("serializable receipt");
    DisableRecord::Receipt {
        receipt_digest,
        payload,
    }
}

fn record_key(record: &DisableRecord) -> String {
    match record {
        DisableRecord::Intent { intent_digest, .. } => format!("intent:{intent_digest}"),
        DisableRecord::Receipt { payload, .. } => format!("receipt:{}", payload.intent_digest),
    }
}

fn put_record(store: &ImmutableByteStore, record: &DisableRecord) -> Result<bool, String> {
    validate_record(record)?;
    let bytes = serde_json::to_vec(record).map_err(|error| error.to_string())?;
    store
        .put(&record_key(record), &bytes)
        .map(|outcome| outcome == StoreWriteOutcome::AlreadyPresent)
        .map_err(|error| error.to_string())
}

fn validate_record(record: &DisableRecord) -> Result<(), String> {
    match record {
        DisableRecord::Intent {
            intent_digest,
            payload,
        } => {
            validate_payload(payload)?;
            if &canonical_digest(INTENT_DOMAIN, payload)? != intent_digest {
                return Err("custody disable intent digest mismatch".to_owned());
            }
        }
        DisableRecord::Receipt {
            receipt_digest,
            payload,
        } => {
            if payload.schema_version != RECORD_SCHEMA_VERSION
                || payload.sequence == 0
                || payload.outcome != "disabled"
                || !is_digest(&payload.intent_digest)
                || !is_digest(&payload.policy_digest)
                || !is_digest(&payload.after_config.sha256)
                || !is_digest(&payload.custody_history.schema_digest)
                || !is_digest(&payload.custody_history.digest)
            {
                return Err("custody disable receipt is invalid".to_owned());
            }
            if &canonical_digest(RECEIPT_DOMAIN, payload)? != receipt_digest {
                return Err("custody disable receipt digest mismatch".to_owned());
            }
        }
    }
    Ok(())
}

fn validate_payload(payload: &DisableIntentPayload) -> Result<(), String> {
    if payload.schema_version != RECORD_SCHEMA_VERSION
        || payload.sequence == 0
        || !is_digest(&payload.policy_digest)
        || !is_digest(&payload.before_config.sha256)
        || !is_digest(&payload.after_config.sha256)
        || !is_digest(&payload.custody_history.schema_digest)
        || !is_digest(&payload.custody_history.digest)
        || !payload.before_config.exists
        || !payload.after_config.exists
    {
        return Err("custody disable intent is invalid".to_owned());
    }
    Ok(())
}

fn load_records(store: &ImmutableByteStore) -> Result<DisableRecordIndex, String> {
    let mut records = DisableRecordIndex::default();
    for result in store
        .list_record_results()
        .map_err(|error| error.to_string())?
    {
        let bytes = result.map_err(|error| error.to_string())?;
        let record: DisableRecord =
            serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        validate_record(&record)?;
        match record {
            DisableRecord::Intent {
                intent_digest,
                payload,
            } => {
                if records.intents.insert(intent_digest, payload).is_some() {
                    return Err("duplicate custody disable intent".to_owned());
                }
            }
            receipt @ DisableRecord::Receipt { .. } => {
                let DisableRecord::Receipt { payload, .. } = &receipt else {
                    unreachable!()
                };
                if records
                    .receipts
                    .insert(payload.intent_digest.clone(), receipt)
                    .is_some()
                {
                    return Err("duplicate custody disable receipt".to_owned());
                }
            }
        }
    }
    for (intent_digest, receipt) in &records.receipts {
        let Some(intent) = records.intents.get(intent_digest) else {
            return Err("orphan custody disable receipt".to_owned());
        };
        let DisableRecord::Receipt { payload, .. } = receipt else {
            unreachable!()
        };
        if payload.policy_digest != intent.policy_digest
            || payload.sequence != intent.sequence
            || payload.after_config != intent.after_config
            || payload.custody_history != intent.custody_history
        {
            return Err("custody disable receipt does not bind its intent".to_owned());
        }
    }
    validate_record_chain(&records)?;
    Ok(records)
}

fn validate_record_chain(records: &DisableRecordIndex) -> Result<(), String> {
    let mut ordered = BTreeMap::new();
    for (intent_digest, intent) in &records.intents {
        if ordered.insert(intent.sequence, intent_digest).is_some() {
            return Err("duplicate custody disable sequence".to_owned());
        }
    }
    for (offset, (sequence, intent_digest)) in ordered.iter().enumerate() {
        let expected_sequence = u64::try_from(offset)
            .map_err(|_| "custody disable sequence overflow".to_owned())?
            .checked_add(1)
            .ok_or_else(|| "custody disable sequence overflow".to_owned())?;
        if *sequence != expected_sequence {
            return Err("custody disable sequence is not contiguous".to_owned());
        }
        let intent = records
            .intents
            .get(*intent_digest)
            .ok_or_else(|| "custody disable sequence lost its intent".to_owned())?;
        if offset == 0 {
            if intent.predecessor_receipt_digest.is_some() {
                return Err("first custody disable intent has a predecessor".to_owned());
            }
            continue;
        }
        let prior_digest = ordered
            .get(&(sequence - 1))
            .ok_or_else(|| "custody disable predecessor is missing".to_owned())?;
        let prior_receipt = records
            .receipts
            .get(*prior_digest)
            .ok_or_else(|| "custody disable intent follows an unresolved generation".to_owned())?;
        let DisableRecord::Receipt { receipt_digest, .. } = prior_receipt else {
            unreachable!()
        };
        if intent.predecessor_receipt_digest.as_deref() != Some(receipt_digest) {
            return Err("custody disable predecessor receipt mismatch".to_owned());
        }
    }
    Ok(())
}

fn pending_intent(records: &DisableRecordIndex) -> Result<Option<&DisableIntentPayload>, String> {
    let mut pending = records
        .intents
        .iter()
        .filter(|(digest, _)| !records.receipts.contains_key(*digest))
        .map(|(_, intent)| intent);
    let first = pending.next();
    if pending.next().is_some() {
        return Err("ambiguous custody disable recovery intents".to_owned());
    }
    Ok(first)
}

fn next_intent_generation(records: &DisableRecordIndex) -> Result<(u64, Option<String>), String> {
    if pending_intent(records)?.is_some() {
        return Err("an unresolved custody disable generation already exists".to_owned());
    }
    let sequence = u64::try_from(records.intents.len())
        .map_err(|_| "custody disable sequence overflow".to_owned())?
        .checked_add(1)
        .ok_or_else(|| "custody disable sequence overflow".to_owned())?;
    let predecessor = if sequence == 1 {
        None
    } else {
        let (intent_digest, _) = records
            .intents
            .iter()
            .max_by_key(|(_, intent)| intent.sequence)
            .ok_or_else(|| "custody disable predecessor is missing".to_owned())?;
        let receipt = records
            .receipts
            .get(intent_digest)
            .ok_or_else(|| "custody disable predecessor receipt is missing".to_owned())?;
        let DisableRecord::Receipt { receipt_digest, .. } = receipt else {
            unreachable!()
        };
        Some(receipt_digest.clone())
    };
    Ok((sequence, predecessor))
}

fn recover_enabled_intent(
    records: &DisableRecordIndex,
    policy_digest: &str,
    before_config: &ConfigFence,
    after_config: &ConfigFence,
    history: &CustodyHistoryFence,
) -> Result<Option<DisableIntentPayload>, String> {
    if let Some(intent) = pending_intent(records)? {
        if intent.policy_digest != policy_digest {
            return Err(
                "unresolved custody disable generation belongs to another policy".to_owned(),
            );
        }
        if &intent.before_config != before_config || &intent.after_config != after_config {
            return Err("custody disable config changed after durable intent".to_owned());
        }
        if &intent.custody_history != history {
            return Err("custody history changed after durable intent".to_owned());
        }
        return Ok(Some(intent.clone()));
    }
    Ok(None)
}

fn recover_disabled_intent(
    records: &DisableRecordIndex,
    policy_digest: &str,
    config: &ConfigFence,
    history: &CustodyHistoryFence,
) -> Result<Option<(DisableIntentPayload, Option<DisableRecord>)>, String> {
    if let Some(intent) = pending_intent(records)? {
        if intent.policy_digest != policy_digest {
            return Err(
                "unresolved custody disable generation belongs to another policy".to_owned(),
            );
        }
        if &intent.after_config != config {
            return Err("custody disable config changed after durable intent".to_owned());
        }
        if &intent.custody_history != history {
            return Err("custody history changed after durable intent".to_owned());
        }
        return Ok(Some((intent.clone(), None)));
    }

    let completed = records
        .intents
        .iter()
        .filter_map(|(digest, intent)| {
            let receipt = records.receipts.get(digest)?;
            (intent.policy_digest == policy_digest
                && &intent.after_config == config
                && &intent.custody_history == history)
                .then(|| (intent.clone(), receipt.clone()))
        })
        .max_by_key(|(intent, _)| intent.sequence);
    Ok(completed.map(|(intent, receipt)| (intent, Some(receipt))))
}

fn canonical_digest(domain: &[u8], value: &impl Serialize) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    let mut digest = Sha256::new();
    digest.update(
        u64::try_from(domain.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    digest.update(domain);
    digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(bytes);
    Ok(hex::encode(digest.finalize()))
}

fn config_fence(text: Option<&str>) -> ConfigFence {
    ConfigFence {
        exists: text.is_some(),
        sha256: hex::encode(Sha256::digest(text.unwrap_or_default().as_bytes())),
    }
}

fn custody_history_fence(
    state_dir: &Path,
    snapshot_barrier: Option<&crate::writer_domain_lease::ProductionSnapshotLease>,
) -> Result<CustodyHistoryFence, &'static str> {
    let inspected = WorkLedger::inspect_existing_verified_custody_read_only(
        state_dir,
        snapshot_barrier,
        |connection, schema_digest| Ok(inspect_custody_history(connection, schema_digest)),
    )
    .map_err(|error| classify_ledger_error(&error))?;
    let Some(inspected) = inspected else {
        return Ok(empty_history_fence());
    };
    let (active, history) = inspected?;
    if active != 0 {
        return Err("custody-state-active");
    }
    Ok(history)
}

fn classify_ledger_error(error: &WorkLedgerError) -> &'static str {
    match error {
        WorkLedgerError::UnsupportedSchema(_) | WorkLedgerError::ForeignSchemaLineage { .. } => {
            "custody-state-schema-mismatch"
        }
        WorkLedgerError::Refused(_) => "custody-state-invalid",
        WorkLedgerError::Io(_) | WorkLedgerError::Sql(_) | WorkLedgerError::Json { .. } => {
            "custody-state-unavailable"
        }
    }
}

fn inspect_custody_history(
    connection: &Connection,
    schema_digest: &str,
) -> Result<(u64, CustodyHistoryFence), &'static str> {
    if !is_digest(schema_digest) {
        return Err("custody-state-schema-mismatch");
    }
    let schema_version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(|_| "custody-state-unavailable")?;
    Ok((
        active_custody_rows(connection)?,
        digest_history(connection, schema_version, schema_digest)?,
    ))
}

fn active_custody_rows(connection: &Connection) -> Result<u64, &'static str> {
    let mut active = 0_u64;
    for (table, predicate) in [
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
    ] {
        let count: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {predicate}"),
                [],
                |row| row.get(0),
            )
            .map_err(|_| "custody-state-unavailable")?;
        active = active
            .checked_add(u64::try_from(count).map_err(|_| "custody-state-invalid")?)
            .ok_or("custody-state-invalid")?;
    }
    Ok(active)
}

fn digest_history(
    connection: &Connection,
    schema_version: i64,
    schema_digest: &str,
) -> Result<CustodyHistoryFence, &'static str> {
    let mut digest = Sha256::new();
    digest.update(HISTORY_DOMAIN);
    digest.update(schema_version.to_be_bytes());
    encode_bytes(&mut digest, schema_digest.as_bytes());
    let mut rows = 0_u64;
    for table in CUSTODY_TABLES {
        encode_bytes(&mut digest, table.as_bytes());
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table}"))
            .map_err(|_| "custody-state-unavailable")?;
        let column_count = statement.column_count();
        let mut encoded_rows = Vec::new();
        let mut query = statement
            .query([])
            .map_err(|_| "custody-state-unavailable")?;
        while let Some(row) = query.next().map_err(|_| "custody-state-unavailable")? {
            let mut encoded = Vec::new();
            for index in 0..column_count {
                encode_value(
                    &mut encoded,
                    row.get_ref(index)
                        .map_err(|_| "custody-state-unavailable")?,
                );
            }
            encoded_rows.push(encoded);
        }
        encoded_rows.sort();
        let table_rows = u64::try_from(encoded_rows.len()).map_err(|_| "custody-state-invalid")?;
        rows = rows
            .checked_add(table_rows)
            .ok_or("custody-state-invalid")?;
        digest.update(table_rows.to_be_bytes());
        for row in encoded_rows {
            encode_bytes(&mut digest, &row);
        }
    }
    Ok(CustodyHistoryFence {
        ledger_exists: true,
        schema_version,
        schema_digest: schema_digest.to_owned(),
        rows,
        digest: hex::encode(digest.finalize()),
    })
}

fn empty_history_fence() -> CustodyHistoryFence {
    let schema_digest = hex::encode(Sha256::digest(
        b"shipyard.custody-disable.schema.absent.v1\0",
    ));
    let mut digest = Sha256::new();
    digest.update(HISTORY_DOMAIN);
    digest.update(0_i64.to_be_bytes());
    encode_bytes(&mut digest, schema_digest.as_bytes());
    CustodyHistoryFence {
        ledger_exists: false,
        schema_version: 0,
        schema_digest,
        rows: 0,
        digest: hex::encode(digest.finalize()),
    }
}

fn encode_bytes(digest: &mut Sha256, bytes: &[u8]) {
    digest.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    digest.update(bytes);
}

fn encode_value(output: &mut Vec<u8>, value: ValueRef<'_>) {
    match value {
        ValueRef::Null => output.push(0),
        ValueRef::Integer(value) => {
            output.push(1);
            output.extend_from_slice(&value.to_be_bytes());
        }
        ValueRef::Real(value) => {
            output.push(2);
            output.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        ValueRef::Text(value) => {
            output.push(3);
            output.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            output.extend_from_slice(value);
        }
        ValueRef::Blob(value) => {
            output.push(4);
            output.extend_from_slice(&u64::try_from(value.len()).unwrap_or(u64::MAX).to_be_bytes());
            output.extend_from_slice(value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf, String) {
        let root = tempfile::tempdir().expect("root");
        let global = root.path().join("global");
        let state = root.path().join("state");
        fs::create_dir_all(&global).expect("global");
        #[cfg(unix)]
        fs::set_permissions(&global, fs::Permissions::from_mode(0o700)).expect("global mode");
        let config = "[other]\nvalue = 7\n\n[custody_transport]\nenabled = true\nlocal_machine_ref = \"machine_test\"\nlocal_incarnation_ref = \"incarnation_test\"\nlocal_route_ref = \"route_test\"\nauthority_digest = \"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"\n";
        let config_path = global.join("config.toml");
        fs::write(&config_path, config).expect("config");
        #[cfg(unix)]
        fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).expect("config mode");
        let policy = parse_existing_policy(config)
            .expect("parse")
            .expect("policy");
        let digest = policy_digest(&policy).expect("digest");
        (root, global, state, digest)
    }

    fn committed_records(state: &Path) -> Vec<Vec<u8>> {
        ImmutableByteStore::open_read_only(record_store_path(state), MAX_RECORD_BYTES)
            .expect("store")
            .list_record_results()
            .expect("records")
            .into_iter()
            .collect::<Result<Vec<_>, _>>()
            .expect("record bytes")
    }

    fn create_ledger(state: &Path) {
        drop(WorkLedger::open(state).expect("ledger"));
    }

    fn insert_control(state: &Path, suffix: &str, control_state: &str) {
        let connection = Connection::open(WorkLedger::path_at(state)).expect("connection");
        connection
            .execute(
                "INSERT INTO custody_controls (
                   control_id, message_id, identity_digest, kind, successor_message_id,
                   expected_rebind_epoch, workstream_revision, authority_digest,
                   control_digest, state, receipt_digest, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, 'cancelled', NULL, 1, 1, ?4, ?5, ?6, ?7, ?8, ?8)",
                rusqlite::params![
                    format!("cc_{suffix}"),
                    format!("wm_{suffix}"),
                    "a".repeat(64),
                    "b".repeat(64),
                    if suffix == "one" {
                        "c".repeat(64)
                    } else {
                        "d".repeat(64)
                    },
                    control_state,
                    (control_state == "acknowledged").then(|| "e".repeat(64)),
                    "2026-09-02T00:00:00Z",
                ],
            )
            .expect("insert custody control");
    }

    fn put_raw_record(state: &Path, key: &str, bytes: &[u8]) {
        let store = open_store(state).expect("store");
        assert_eq!(
            store.put(key, bytes).expect("put raw record"),
            StoreWriteOutcome::Created
        );
    }

    #[cfg(unix)]
    fn write_private(path: &Path, bytes: impl AsRef<[u8]>) {
        fs::write(path, bytes).expect("private file");
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).expect("private mode");
    }

    #[cfg(unix)]
    fn tree_snapshot(root: &Path) -> Vec<(PathBuf, u64, u32, Vec<u8>)> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt as _};

        fn visit(root: &Path, path: &Path, entries: &mut Vec<(PathBuf, u64, u32, Vec<u8>)>) {
            let mut children = fs::read_dir(path)
                .expect("read snapshot directory")
                .collect::<Result<Vec<_>, _>>()
                .expect("snapshot entries");
            children.sort_by_key(std::fs::DirEntry::file_name);
            for child in children {
                let child_path = child.path();
                let metadata = fs::symlink_metadata(&child_path).expect("snapshot metadata");
                let relative = child_path
                    .strip_prefix(root)
                    .expect("snapshot relative path")
                    .to_path_buf();
                let bytes = if metadata.is_file() {
                    fs::read(&child_path).expect("snapshot bytes")
                } else {
                    Vec::new()
                };
                entries.push((
                    relative,
                    metadata.ino(),
                    metadata.permissions().mode() & 0o777,
                    bytes,
                ));
                if metadata.is_dir() {
                    visit(root, &child_path, entries);
                }
            }
        }

        let mut entries = Vec::new();
        visit(root, root, &mut entries);
        entries
    }

    #[test]
    fn every_interruption_checkpoint_replays_one_exact_receipt() {
        for checkpoint in [
            DisableCheckpoint::AfterIntent,
            DisableCheckpoint::AfterConfig,
            DisableCheckpoint::AfterReceipt,
        ] {
            let (_root, global, state, digest) = fixture();
            let interrupted = disable_with_checkpoint(&global, &state, &digest, true, checkpoint);
            assert_eq!(
                interrupted.reason_code.as_deref(),
                Some("custody-disable-interrupted")
            );
            let replay = disable(&global, &state, &digest, true);
            assert_eq!(replay.outcome, "disabled");
            let receipt = replay.receipt_digest.expect("receipt");
            assert!(is_digest(&receipt));
            let records = committed_records(&state);
            assert_eq!(records.len(), 2);
            let second = disable(&global, &state, &digest, true);
            assert_eq!(second.receipt_digest.as_deref(), Some(receipt.as_str()));
            assert_eq!(committed_records(&state), records);
        }
    }

    #[cfg(unix)]
    #[test]
    fn supported_reprovision_creates_a_new_chained_disable_generation() {
        let root = tempfile::tempdir().expect("root");
        let owner = root.path().join("owner");
        fs::create_dir(&owner).expect("owner");
        let manifest = super::super::tests::fixture(&owner);
        let input = owner.join("custody.toml");
        write_private(&input, manifest);
        let global = root.path().join("global");
        let state = root.path().join("state");

        let first_install = super::super::provision_with_state(&global, &state, &input, true);
        assert_eq!(first_install.outcome, "applied");
        let digest = first_install.policy_digest.expect("policy digest");
        let first_disable = disable(&global, &state, &digest, true);
        assert_eq!(first_disable.outcome, "disabled");
        let first_receipt = first_disable.receipt_digest.expect("first receipt");

        let second_install = super::super::provision_with_state(&global, &state, &input, true);
        assert_eq!(second_install.outcome, "applied");
        let second_disable = disable(&global, &state, &digest, true);
        assert_eq!(second_disable.outcome, "disabled");
        let second_receipt = second_disable.receipt_digest.expect("second receipt");

        assert_ne!(second_receipt, first_receipt);
        let records = load_records(
            &ImmutableByteStore::open_read_only(record_store_path(&state), MAX_RECORD_BYTES)
                .expect("store"),
        )
        .expect("records");
        assert_eq!(records.intents.len(), 2);
        assert_eq!(records.receipts.len(), 2);
        let mut intents = records.intents.values().collect::<Vec<_>>();
        intents.sort_by_key(|intent| intent.sequence);
        assert_eq!(intents[0].sequence, 1);
        assert_eq!(intents[0].predecessor_receipt_digest, None);
        assert_eq!(intents[1].sequence, 2);
        assert_eq!(
            intents[1].predecessor_receipt_digest.as_deref(),
            Some(first_receipt.as_str())
        );
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_generation_fences_supported_cross_policy_provision() {
        let root = tempfile::tempdir().expect("root");
        let owner_a = root.path().join("owner-a");
        let owner_b = root.path().join("owner-b");
        fs::create_dir(&owner_a).expect("owner a");
        fs::create_dir(&owner_b).expect("owner b");
        let input_a = owner_a.join("custody.toml");
        let input_b = owner_b.join("custody.toml");
        write_private(&input_a, super::super::tests::fixture(&owner_a));
        write_private(&input_b, super::super::tests::fixture(&owner_b));
        let global = root.path().join("global");
        let state = root.path().join("state");

        let install_a = super::super::provision_with_state(&global, &state, &input_a, true);
        assert_eq!(install_a.outcome, "applied");
        let digest_a = install_a.policy_digest.expect("policy a");
        let interrupted = disable_with_checkpoint(
            &global,
            &state,
            &digest_a,
            true,
            DisableCheckpoint::AfterConfig,
        );
        assert_eq!(
            interrupted.reason_code.as_deref(),
            Some("custody-disable-interrupted")
        );

        for apply in [false, true] {
            let blocked_same = super::super::provision_with_state(&global, &state, &input_a, apply);
            assert_eq!(
                blocked_same.reason_code.as_deref(),
                Some("custody-disable-generation-unresolved")
            );
        }

        let blocked_plan = super::super::provision_with_state(&global, &state, &input_b, false);
        assert_eq!(
            blocked_plan.reason_code.as_deref(),
            Some("custody-disable-generation-unresolved")
        );
        let blocked = super::super::provision_with_state(&global, &state, &input_b, true);
        assert_eq!(
            blocked.reason_code.as_deref(),
            Some("custody-disable-generation-unresolved")
        );
        assert!(
            !fs::read_to_string(global.join("config.toml"))
                .expect("config")
                .contains("[custody_transport]")
        );
        assert_eq!(committed_records(&state).len(), 1);

        assert_eq!(
            disable(&global, &state, &digest_a, true).outcome,
            "disabled"
        );
        let install_b = super::super::provision_with_state(&global, &state, &input_b, true);
        assert_eq!(install_b.outcome, "applied");
        let digest_b = install_b.policy_digest.expect("policy b");
        assert_ne!(digest_b, digest_a);
        assert_eq!(
            disable(&global, &state, &digest_b, true).outcome,
            "disabled"
        );
        assert_eq!(committed_records(&state).len(), 4);
    }

    #[test]
    fn dry_run_and_wrong_digest_create_no_receipt_store() {
        let (_root, global, state, digest) = fixture();
        assert_eq!(
            disable(&global, &state, &digest, false).outcome,
            "disable_planned"
        );
        assert!(!record_store_path(&state).exists());
        assert_eq!(
            disable(&global, &state, &"b".repeat(64), true).outcome,
            "refused"
        );
        assert!(!record_store_path(&state).exists());
        assert!(
            fs::read_to_string(global.join("config.toml"))
                .expect("config")
                .contains("[custody_transport]")
        );
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_with_existing_ledger_is_inode_name_and_byte_stable() {
        let (_root, global, state, digest) = fixture();
        create_ledger(&state);
        let before = tree_snapshot(&state);

        let planned = disable(&global, &state, &digest, false);

        assert_eq!(planned.outcome, "disable_planned");
        assert_eq!(tree_snapshot(&state), before);
        assert!(!state.join(".sandbox-writer-domain.lock").exists());
        assert!(!state.join(".sandbox-writer-domain.turnstile.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn dry_run_refuses_orphan_wal_without_creating_shared_memory() {
        let (_root, global, state, digest) = fixture();
        create_ledger(&state);
        let database = WorkLedger::path_at(&state);
        let live = Connection::open(&database).expect("live connection");
        live.execute_batch("PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;")
            .expect("retain WAL");
        insert_control(&state, "wal", "acknowledged");
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        let shm = PathBuf::from(format!("{}-shm", database.display()));
        assert!(wal.exists());
        fs::remove_file(&shm).expect("remove shared memory fixture");
        let before = tree_snapshot(&state);

        let refused = disable(&global, &state, &digest, false);

        assert_eq!(refused.outcome, "refused");
        assert_eq!(tree_snapshot(&state), before);
        assert!(!shm.exists());
        drop(live);
    }

    #[test]
    fn already_absent_apply_does_not_create_receipt_state() {
        let (_root, global, state, digest) = fixture();
        fs::remove_file(global.join("config.toml")).expect("remove config");

        let report = disable(&global, &state, &digest, true);

        assert_eq!(report.outcome, "disabled");
        assert!(!state.exists());
        assert!(!record_store_path(&state).exists());
    }

    #[test]
    fn read_only_default_off_proof_returns_exact_receipt() {
        let (_root, global, state, digest) = fixture();
        let applied = disable(&global, &state, &digest, true);
        let receipt = applied.receipt_digest.expect("receipt");
        let records = committed_records(&state);
        let proof = disable(&global, &state, &digest, false);
        assert_eq!(proof.outcome, "disabled");
        assert_eq!(proof.receipt_digest.as_deref(), Some(receipt.as_str()));
        assert_eq!(committed_records(&state), records);
    }

    #[test]
    fn terminal_custody_history_survives_disable_exactly() {
        let (_root, global, state, digest) = fixture();
        create_ledger(&state);
        insert_control(&state, "one", "acknowledged");
        let before = custody_history_fence(&state, None).expect("before history");

        let applied = disable(&global, &state, &digest, true);

        assert_eq!(applied.outcome, "disabled");
        assert!(applied.receipt_digest.as_deref().is_some_and(is_digest));
        assert_eq!(
            custody_history_fence(&state, None).expect("after history"),
            before
        );
        let connection = Connection::open(WorkLedger::path_at(&state)).expect("connection");
        let row: (String, String, String) = connection
            .query_row(
                "SELECT state, receipt_digest, updated_at
                   FROM custody_controls WHERE control_id = 'cc_one'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("retained control");
        assert_eq!(
            row,
            (
                "acknowledged".to_owned(),
                "e".repeat(64),
                "2026-09-02T00:00:00Z".to_owned(),
            )
        );
    }

    #[test]
    fn migrated_production_ledgers_match_the_canonical_custody_schema() {
        for reconstruct in [
            crate::work_ledger::reconstruct_authentic_v10_schema_for_test
                as fn(&Connection) -> crate::work_ledger::WorkLedgerResult<()>,
            crate::work_ledger::reconstruct_authentic_v11_schema_for_test,
            crate::work_ledger::reconstruct_authentic_v12_schema_for_test,
        ] {
            let (_root, global, state, digest) = fixture();
            create_ledger(&state);
            let database = WorkLedger::path_at(&state);
            let connection = Connection::open(&database).expect("historical ledger");
            reconstruct(&connection).expect("authentic historical schema");
            drop(connection);
            drop(WorkLedger::open(&state).expect("migrate historical ledger"));

            let applied = disable(&global, &state, &digest, true);

            assert_eq!(applied.outcome, "disabled");
            assert!(applied.receipt_digest.as_deref().is_some_and(is_digest));
        }
    }

    #[test]
    fn active_custody_state_refuses_before_intent_or_config_write() {
        let (_root, global, state, digest) = fixture();
        create_ledger(&state);
        insert_control(&state, "one", "pending");
        let before = fs::read(global.join("config.toml")).expect("config");

        let refused = disable(&global, &state, &digest, true);

        assert_eq!(refused.outcome, "refused");
        assert_eq!(refused.reason_code.as_deref(), Some("custody-state-active"));
        assert_eq!(
            fs::read(global.join("config.toml")).expect("config"),
            before
        );
        assert!(!record_store_path(&state).exists());
    }

    #[test]
    fn pending_intent_with_absent_config_is_read_only_planned_then_completed() {
        let (_root, global, state, digest) = fixture();
        let interrupted = disable_with_checkpoint(
            &global,
            &state,
            &digest,
            true,
            DisableCheckpoint::AfterConfig,
        );
        assert_eq!(interrupted.outcome, "refused");
        let intent_only = committed_records(&state);
        assert_eq!(intent_only.len(), 1);

        let planned = disable(&global, &state, &digest, false);
        assert_eq!(planned.outcome, "disable_recovery_planned");
        assert_eq!(committed_records(&state), intent_only);

        let applied = disable(&global, &state, &digest, true);
        assert_eq!(applied.outcome, "disabled");
        assert!(applied.receipt_digest.as_deref().is_some_and(is_digest));
        assert_eq!(committed_records(&state).len(), 2);
    }

    #[test]
    fn history_drift_after_durable_intent_refuses_completion() {
        let (_root, global, state, digest) = fixture();
        create_ledger(&state);
        insert_control(&state, "one", "acknowledged");
        let interrupted = disable_with_checkpoint(
            &global,
            &state,
            &digest,
            true,
            DisableCheckpoint::AfterIntent,
        );
        assert_eq!(interrupted.outcome, "refused");
        insert_control(&state, "two", "acknowledged");

        let refused = disable(&global, &state, &digest, true);

        assert_eq!(refused.outcome, "refused");
        assert_eq!(committed_records(&state).len(), 1);
        assert!(
            fs::read_to_string(global.join("config.toml"))
                .expect("config")
                .contains("[custody_transport]")
        );
    }

    #[test]
    fn unsupported_or_drifted_custody_schema_refuses_before_intent() {
        for mutation in [
            "PRAGMA user_version = 999;",
            "DROP TRIGGER ledger_schema_identity_immutable;",
            "DROP TABLE custody_events;",
            "DROP TRIGGER custody_event_no_delete;",
            "DROP TRIGGER custody_successor_state_transition_fence;",
            "ALTER TABLE custody_events ADD COLUMN surprise TEXT;",
            "CREATE TABLE custody_surprise (value TEXT);",
            "PRAGMA writable_schema = ON;
             UPDATE sqlite_schema
                SET sql = replace(sql,
                    'sequence INTEGER NOT NULL CHECK(sequence > 0)',
                    'sequence INTEGER NOT NULL')
              WHERE type = 'table' AND name = 'custody_events';
             PRAGMA writable_schema = OFF;
             PRAGMA schema_version = 999;",
        ] {
            let (_root, global, state, digest) = fixture();
            create_ledger(&state);
            let before = fs::read(global.join("config.toml")).expect("config");
            Connection::open(WorkLedger::path_at(&state))
                .expect("connection")
                .execute_batch(mutation)
                .expect("schema mutation");

            let refused = disable(&global, &state, &digest, true);

            assert_eq!(refused.outcome, "refused");
            assert!(matches!(
                refused.reason_code.as_deref(),
                Some("custody-state-schema-mismatch" | "custody-state-invalid")
            ));
            assert!(!record_store_path(&state).exists());
            assert_eq!(
                fs::read(global.join("config.toml")).expect("config"),
                before
            );
        }
    }

    #[test]
    fn malformed_or_unbound_receipt_store_refuses_without_config_write() {
        {
            let (_root, global, state, digest) = fixture();
            let before = fs::read(global.join("config.toml")).expect("config");
            put_raw_record(&state, "malformed", b"{}");
            let refused = disable(&global, &state, &digest, true);
            assert_eq!(refused.outcome, "refused");
            assert_eq!(
                fs::read(global.join("config.toml")).expect("config"),
                before
            );
        }
        {
            let (_root, global, state, digest) = fixture();
            let before = fs::read(global.join("config.toml")).expect("config");
            let intent = DisableIntentPayload {
                schema_version: RECORD_SCHEMA_VERSION,
                sequence: 1,
                predecessor_receipt_digest: None,
                policy_digest: digest.clone(),
                local_machine_ref: None,
                local_incarnation_ref: None,
                local_route_ref: None,
                authority_digest: None,
                before_config: config_fence(Some("before")),
                after_config: config_fence(Some("after")),
                custody_history: empty_history_fence(),
            };
            let orphan = receipt_record(&intent);
            put_raw_record(
                &state,
                "orphan",
                &serde_json::to_vec(&orphan).expect("receipt json"),
            );
            let refused = disable(&global, &state, &digest, true);
            assert_eq!(refused.outcome, "refused");
            assert_eq!(
                fs::read(global.join("config.toml")).expect("config"),
                before
            );
        }
    }

    #[test]
    fn wrong_binding_receipt_is_refused() {
        {
            let (_root, global, state, digest) = fixture();
            let interrupted = disable_with_checkpoint(
                &global,
                &state,
                &digest,
                true,
                DisableCheckpoint::AfterIntent,
            );
            assert_eq!(interrupted.outcome, "refused");
            let intent_record: DisableRecord =
                serde_json::from_slice(committed_records(&state).first().expect("intent"))
                    .expect("intent json");
            let DisableRecord::Intent {
                intent_digest,
                payload: intent,
            } = intent_record
            else {
                panic!("expected intent")
            };
            let payload = DisableReceiptPayload {
                schema_version: RECORD_SCHEMA_VERSION,
                sequence: intent.sequence,
                intent_digest,
                policy_digest: "f".repeat(64),
                after_config: intent.after_config,
                custody_history: intent.custody_history,
                outcome: "disabled".to_owned(),
            };
            let wrong = DisableRecord::Receipt {
                receipt_digest: canonical_digest(RECEIPT_DOMAIN, &payload).expect("digest"),
                payload,
            };
            put_raw_record(
                &state,
                "wrong-binding",
                &serde_json::to_vec(&wrong).expect("receipt json"),
            );
            assert_eq!(disable(&global, &state, &digest, true).outcome, "refused");
        }
    }

    #[test]
    fn recovery_refuses_config_drift_after_durable_intent() {
        let (_root, global, state, digest) = fixture();
        let interrupted = disable_with_checkpoint(
            &global,
            &state,
            &digest,
            true,
            DisableCheckpoint::AfterIntent,
        );
        assert_eq!(interrupted.outcome, "refused");
        let path = global.join("config.toml");
        let drifted = fs::read_to_string(&path)
            .expect("config")
            .replace("value = 7", "value = 8");
        fs::write(&path, drifted).expect("drift");
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("mode");
        let refused = disable(&global, &state, &digest, true);
        assert_eq!(refused.outcome, "refused");
        assert!(refused.receipt_digest.is_none());
        assert!(
            fs::read_to_string(path)
                .expect("config")
                .contains("[custody_transport]")
        );
    }

    #[test]
    fn pending_intent_with_enabled_config_has_typed_read_only_plan() {
        let (_root, global, state, digest) = fixture();
        let interrupted = disable_with_checkpoint(
            &global,
            &state,
            &digest,
            true,
            DisableCheckpoint::AfterIntent,
        );
        assert_eq!(interrupted.outcome, "refused");
        let intent_only = committed_records(&state);

        let planned = disable(&global, &state, &digest, false);

        assert_eq!(planned.outcome, "disable_recovery_planned");
        assert_eq!(committed_records(&state), intent_only);
        assert!(
            fs::read_to_string(global.join("config.toml"))
                .expect("config")
                .contains("[custody_transport]")
        );
    }
}
