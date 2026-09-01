//! Schema and provenance validation for legacy lifecycle records.

use super::{
    Deserialize, QUEUE_ABSENT_RECOVERY_SCHEMA_VERSION, QUEUED_EXECUTION_SCHEMA_VERSION,
    QueueAbsentRecoveryRecord, QueuedExecutionEnvelope, QueuedExecutionOutcome,
    RECOVERY_SCHEMA_VERSION, RecoveryRecord, SHIP_STATE_SCHEMA_VERSION, ShipState, Value,
    WorkLedgerError, WorkLedgerResult, validate_queued_execution_envelope,
    validate_queued_execution_outcome, validate_record, validate_recovery_record,
};
#[cfg(unix)]
use crate::queue_request::{QueueRequestError, decode_queued_execution_request_bytes_for_import};

#[derive(Deserialize)]
struct LegacyTerminalHandoff {
    dedupe_key: String,
    repo: String,
    base: String,
    pr_number: u64,
    head_sha: String,
    outcome: String,
    phase: String,
}

#[derive(Deserialize)]
struct LegacyResumeRecord {
    schema_version: u32,
    resume_id: String,
    terminal_handoff_key: String,
    repo: String,
    base: String,
    pr_number: u64,
    head_sha: String,
    dispatch_enabled: bool,
    phase: String,
}

#[cfg(unix)]
pub(super) fn validate_legacy_record_bytes_before_projection(
    kind: &str,
    source: &str,
    path: &std::path::Path,
    bytes: &[u8],
) -> WorkLedgerResult<()> {
    if kind != "queue_request" {
        return Ok(());
    }
    let authoritative_filename =
        path.file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                WorkLedgerError::Refused(format!(
                    "legacy source {source} has a non-UTF-8 authoritative queue filename"
                ))
            })?;
    decode_queued_execution_request_bytes_for_import(bytes, authoritative_filename)
        .map(|_| ())
        .map_err(|error| match error {
            QueueRequestError::Json(error) => WorkLedgerError::Json {
                source: source.to_owned(),
                error,
            },
            error => WorkLedgerError::Refused(format!(
                "legacy source {source} has invalid queue-request authority: {error}"
            )),
        })
}

pub(in crate::work_ledger) fn validate_legacy_record(
    kind: &str,
    source: &str,
    value: &Value,
) -> WorkLedgerResult<()> {
    let invalid = || {
        WorkLedgerError::Refused(format!(
            "legacy source {source} does not match the supported {kind} schema"
        ))
    };
    match kind {
        "ship_state" => {
            let record: ShipState = serde_json::from_value(value.clone()).map_err(|_| invalid())?;
            if record.schema_version != SHIP_STATE_SCHEMA_VERSION
                || record.pr == 0
                || record.repo.split('/').count() != 2
                || record.base_branch.is_empty()
                || record.head_sha.len() != 40
                || !record.head_sha.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(invalid());
            }
        }
        "queue_request" => {
            let record: QueuedExecutionEnvelope =
                serde_json::from_value(value.clone()).map_err(|_| invalid())?;
            if !(1..=QUEUED_EXECUTION_SCHEMA_VERSION).contains(&record.schema_version) {
                return Err(invalid());
            }
            validate_queued_execution_envelope(record).map_err(|error| {
                WorkLedgerError::Refused(format!(
                    "legacy source {source} has invalid queue-request authority: {error}"
                ))
            })?;
        }
        "queue_outcome" => {
            let record: QueuedExecutionOutcome =
                serde_json::from_value(value.clone()).map_err(|_| invalid())?;
            if !(1..=QUEUED_EXECUTION_SCHEMA_VERSION).contains(&record.schema_version())
                || validate_queued_execution_outcome(&record).is_err()
            {
                return Err(invalid());
            }
        }
        "recovery" => {
            let worker = serde_json::from_value::<RecoveryRecord>(value.clone())
                .ok()
                .filter(|record| {
                    record.schema_version == RECOVERY_SCHEMA_VERSION
                        && validate_record(record).is_ok()
                });
            let queue_absent = serde_json::from_value::<QueueAbsentRecoveryRecord>(value.clone())
                .ok()
                .filter(|record| {
                    record.schema_version == QUEUE_ABSENT_RECOVERY_SCHEMA_VERSION
                        && validate_recovery_record(record).is_ok()
                });
            if worker.is_none() && queue_absent.is_none() {
                return Err(invalid());
            }
        }
        "terminal_handoff" => {
            let record: LegacyTerminalHandoff =
                serde_json::from_value(value.clone()).map_err(|_| invalid())?;
            if record.dedupe_key.is_empty()
                || record.repo.is_empty()
                || record.base.is_empty()
                || record.pr_number == 0
                || record.head_sha.is_empty()
                || !matches!(
                    record.outcome.as_str(),
                    "success_continuation" | "actionable_failure"
                )
                || !matches!(
                    record.phase.as_str(),
                    "pending" | "recorded" | "applied" | "resolved"
                )
            {
                return Err(invalid());
            }
            validate_terminal_handoff_provenance(value).map_err(|()| invalid())?;
        }
        "resume_record" => {
            let record: LegacyResumeRecord =
                serde_json::from_value(value.clone()).map_err(|_| invalid())?;
            if record.schema_version != 1
                || record.resume_id.is_empty()
                || record.terminal_handoff_key.is_empty()
                || record.repo.is_empty()
                || record.base.is_empty()
                || record.pr_number == 0
                || record.head_sha.is_empty()
                || record.dispatch_enabled
                || !matches!(record.phase.as_str(), "recorded" | "resolved")
            {
                return Err(invalid());
            }
            validate_resume_provenance(value).map_err(|()| invalid())?;
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

fn validate_terminal_handoff_provenance(value: &Value) -> Result<(), ()> {
    validate_keys(
        value,
        &[
            "dedupe_key",
            "repo",
            "base",
            "pr_number",
            "head_sha",
            "outcome",
            "trigger",
            "next_action",
            "origin_machine",
            "owner_id",
            "ownership_generation",
            "owner_disposition",
            "owner_route_id",
            "owner_provider",
            "resume_transport",
            "owner_terminal_provenance",
            "provider_route",
            "wake_consumer_available",
            "failure_contexts",
            "phase",
            "created_at",
            "updated_at",
        ],
    )?;
    if let Some(route) = value.get("provider_route") {
        validate_keys(
            route,
            &[
                "profile_digest",
                "integrity_hash",
                "generation",
                "revision",
                "provider",
                "account",
                "model",
            ],
        )?;
        if ["profile_digest", "integrity_hash"].iter().any(|key| {
            route
                .get(*key)
                .and_then(Value::as_str)
                .is_none_or(|digest| {
                    digest.len() != 64
                        || !digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        }) || ["generation", "revision"].iter().any(|key| {
            route
                .get(*key)
                .and_then(Value::as_u64)
                .is_none_or(|value| value == 0)
        }) {
            return Err(());
        }
    }
    Ok(())
}

fn validate_resume_provenance(value: &Value) -> Result<(), ()> {
    validate_keys(
        value,
        &[
            "schema_version",
            "resume_id",
            "terminal_handoff_key",
            "repo",
            "base",
            "pr_number",
            "head_sha",
            "owner_id",
            "ownership_generation",
            "routing_disposition",
            "terminal_adapter",
            "agent_adapter",
            "provider_adapter",
            "dispatch_enabled",
            "phase",
            "created_at",
            "updated_at",
        ],
    )?;
    if let Some(adapter) = value.get("terminal_adapter") {
        validate_keys(adapter, &["kind", "route_id"])?;
        let kind = adapter.get("kind").and_then(Value::as_str).ok_or(())?;
        if !matches!(kind, "cmux" | "herd_r")
            || adapter
                .get("route_id")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(());
        }
    }
    if let Some(adapter) = value.get("agent_adapter") {
        validate_keys(adapter, &["kind", "provider", "transport", "route_id"])?;
        if adapter.get("kind").and_then(Value::as_str) != Some("native")
            || ["provider", "transport", "route_id"].iter().any(|key| {
                adapter
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
            })
        {
            return Err(());
        }
    }
    if let Some(adapter) = value.get("provider_adapter") {
        validate_keys(
            adapter,
            &[
                "kind",
                "profile_digest",
                "integrity_hash",
                "generation",
                "revision",
                "provider",
                "account",
                "model",
            ],
        )?;
        if adapter.get("kind").and_then(Value::as_str) != Some("launch_profile")
            || ["profile_digest", "integrity_hash"].iter().any(|key| {
                adapter
                    .get(*key)
                    .and_then(Value::as_str)
                    .is_none_or(|digest| {
                        digest.len() != 64
                            || !digest
                                .bytes()
                                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                    })
            })
            || ["generation", "revision"].iter().any(|key| {
                adapter
                    .get(*key)
                    .and_then(Value::as_u64)
                    .is_none_or(|value| value == 0)
            })
            || adapter
                .get("provider")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(());
        }
    }
    Ok(())
}

fn validate_keys(value: &Value, allowed: &[&str]) -> Result<(), ()> {
    let object = value.as_object().ok_or(())?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(());
    }
    Ok(())
}

// Legacy lifecycle completion is not proof of PR, product acceptance, or
// continuation completion. Preserve unknown truth instead of inferring it.
