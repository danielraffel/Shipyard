use super::{
    Digest, MAX_BASE_REF_BYTES, MAX_DETAIL_BYTES, MAX_FAILED_CONTEXT_BYTES, MAX_FAILED_CONTEXTS,
    MAX_FAILURE_SUMMARY_BYTES, MAX_GENERATION_BYTES, MAX_LABEL_BYTES, MAX_REPO_BYTES,
    MAX_SIGNATURE_BYTES, RECOVERY_SCHEMA_VERSION, RecoveryError, RecoveryFailureFact,
    RecoveryRecord, RecoveryRequest, RecoveryRequiredCheck, RecoveryResult, RecoveryStatus, Sha256,
};

/// Compute the stable full SHA-256 ID for one recovery identity tuple.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn recovery_id(
    repo: &str,
    pr: u64,
    base_ref: &str,
    head_sha: &str,
    merge_queue: bool,
    opt_out_label: &str,
    failure_fingerprint: &str,
    failure_summary: &str,
    required_checks: &[RecoveryRequiredCheck],
    failure_facts: &[RecoveryFailureFact],
    policy_signature: &str,
) -> String {
    let mut hasher = Sha256::new();
    let pr = pr.to_string();
    hasher.update(RECOVERY_SCHEMA_VERSION.to_be_bytes());
    hasher.update([u8::from(merge_queue)]);
    for component in [
        repo.as_bytes(),
        pr.as_bytes(),
        base_ref.as_bytes(),
        head_sha.as_bytes(),
        opt_out_label.as_bytes(),
        failure_fingerprint.as_bytes(),
        failure_summary.as_bytes(),
        policy_signature.as_bytes(),
    ] {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    hasher.update((required_checks.len() as u64).to_be_bytes());
    for required in required_checks {
        for component in [b"required_check".as_slice(), required.context.as_bytes()] {
            hasher.update((component.len() as u64).to_be_bytes());
            hasher.update(component);
        }
        match required.app_id {
            Some(app_id) => {
                hasher.update(1_u64.to_be_bytes());
                hasher.update(app_id.to_be_bytes());
            }
            None => hasher.update(0_u64.to_be_bytes()),
        }
    }
    hasher.update((failure_facts.len() as u64).to_be_bytes());
    for fact in failure_facts {
        let (kind, value, app_id, conclusion, run_id) = match fact {
            RecoveryFailureFact::MergeState { state } => {
                ("merge_state", state.as_str(), None, None, None)
            }
            RecoveryFailureFact::RequiredCheck {
                context,
                app_id,
                conclusion,
                run_id,
            } => (
                "required_check",
                context.as_str(),
                *app_id,
                Some(conclusion.as_str()),
                *run_id,
            ),
        };
        for component in [
            kind.as_bytes(),
            value.as_bytes(),
            conclusion.unwrap_or_default().as_bytes(),
        ] {
            hasher.update((component.len() as u64).to_be_bytes());
            hasher.update(component);
        }
        match app_id {
            Some(app_id) => {
                hasher.update(1_u64.to_be_bytes());
                hasher.update(app_id.to_be_bytes());
            }
            None => hasher.update(0_u64.to_be_bytes()),
        }
        match run_id {
            Some(run_id) => {
                hasher.update(1_u64.to_be_bytes());
                hasher.update(run_id.to_be_bytes());
            }
            None => hasher.update(0_u64.to_be_bytes()),
        }
    }
    hex::encode(hasher.finalize())
}

pub(super) fn validate_request(request: &RecoveryRequest) -> RecoveryResult<()> {
    if request.schema_version != RECOVERY_SCHEMA_VERSION {
        return Err(RecoveryError::SchemaVersion {
            surface: "request",
            observed: request.schema_version,
        });
    }
    validate_request_fields(
        &request.repo,
        request.pr,
        &request.base_ref,
        &request.head_sha,
        request.merge_queue,
        &request.opt_out_label,
        &request.failure_fingerprint,
        &request.failure_summary,
        &request.required_checks,
        &request.failure_facts,
        &request.policy_signature,
        &request.config_signature,
    )?;
    let expected = recovery_id(
        &request.repo,
        request.pr,
        &request.base_ref,
        &request.head_sha,
        request.merge_queue,
        &request.opt_out_label,
        &request.failure_fingerprint,
        &request.failure_summary,
        &request.required_checks,
        &request.failure_facts,
        &request.policy_signature,
    );
    if request.id != expected {
        return Err(RecoveryError::IdentityCollision(request.id.clone()));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn validate_request_fields(
    repo: &str,
    pr: u64,
    base_ref: &str,
    head_sha: &str,
    _merge_queue: bool,
    opt_out_label: &str,
    failure_fingerprint: &str,
    failure_summary: &str,
    required_checks: &[RecoveryRequiredCheck],
    failure_facts: &[RecoveryFailureFact],
    policy_signature: &str,
    config_signature: &str,
) -> RecoveryResult<()> {
    validate_repo(repo)?;
    if pr == 0 {
        return Err(RecoveryError::InvalidRequest(
            "pull-request number must be positive".to_owned(),
        ));
    }
    validate_base_ref(base_ref)?;
    if head_sha.len() != 40 || !head_sha.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(RecoveryError::InvalidRequest(
            "head_sha must be a full 40-character hexadecimal SHA-1".to_owned(),
        ));
    }
    validate_text("opt_out_label", opt_out_label, 1, MAX_LABEL_BYTES)?;
    if opt_out_label.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RecoveryError::InvalidRequest(
            "opt_out_label must not contain control characters".to_owned(),
        ));
    }
    validate_signature("failure_fingerprint", failure_fingerprint)?;
    validate_text(
        "failure_summary",
        failure_summary,
        1,
        MAX_FAILURE_SUMMARY_BYTES,
    )?;
    validate_required_check_policy(required_checks)?;
    validate_failure_facts(required_checks, failure_facts)?;
    validate_signature("policy_signature", policy_signature)?;
    validate_signature("config_signature", config_signature)
}

fn validate_failure_facts(
    required_checks: &[RecoveryRequiredCheck],
    failure_facts: &[RecoveryFailureFact],
) -> RecoveryResult<()> {
    if failure_facts.is_empty() || failure_facts.len() > MAX_FAILED_CONTEXTS {
        return Err(RecoveryError::InvalidRequest(format!(
            "failure_facts must contain 1..={MAX_FAILED_CONTEXTS} Shipyard-normalized facts"
        )));
    }
    let mut failure_kind = None;
    let mut prior_fact = None;
    for fact in failure_facts {
        if prior_fact.is_some_and(|prior| prior >= fact) {
            return Err(RecoveryError::InvalidRequest(
                "failure_facts must be strictly sorted and unique".to_owned(),
            ));
        }
        let current_kind = match fact {
            RecoveryFailureFact::MergeState { state } => {
                validate_text("merge_state", state, 1, MAX_FAILED_CONTEXT_BYTES)?;
                if !matches!(state.as_str(), "DIRTY" | "CONFLICTING" | "BEHIND") {
                    return Err(RecoveryError::InvalidRequest(
                        "merge_state evidence must name DIRTY, CONFLICTING, or BEHIND".to_owned(),
                    ));
                }
                "merge_state"
            }
            RecoveryFailureFact::RequiredCheck {
                context,
                app_id,
                conclusion,
                run_id,
            } => {
                validate_text("required_check", context, 1, MAX_FAILED_CONTEXT_BYTES)?;
                if app_id == &Some(0) {
                    return Err(RecoveryError::InvalidRequest(
                        "required-check app_id must be positive".to_owned(),
                    ));
                }
                if !required_checks
                    .iter()
                    .any(|required| required.context == *context && required.app_id == *app_id)
                {
                    return Err(RecoveryError::InvalidRequest(
                        "failed required-check fact is absent from the complete policy snapshot"
                            .to_owned(),
                    ));
                }
                validate_text(
                    "required_check conclusion",
                    conclusion,
                    1,
                    MAX_FAILED_CONTEXT_BYTES,
                )?;
                if conclusion != &conclusion.to_ascii_uppercase()
                    || !matches!(
                        conclusion.as_str(),
                        "ACTION_REQUIRED"
                            | "CANCELLED"
                            | "FAILURE"
                            | "STALE"
                            | "STARTUP_FAILURE"
                            | "TIMED_OUT"
                    )
                {
                    return Err(RecoveryError::InvalidRequest(
                        "required-check failure conclusion must be a canonical non-passing terminal value"
                            .to_owned(),
                    ));
                }
                if run_id == &Some(0) {
                    return Err(RecoveryError::InvalidRequest(
                        "required-check failure run_id must be positive".to_owned(),
                    ));
                }
                "required_check"
            }
        };
        if failure_kind.is_some_and(|kind| kind != current_kind) {
            return Err(RecoveryError::InvalidRequest(
                "failure_facts cannot mix merge-state and required-check evidence".to_owned(),
            ));
        }
        failure_kind = Some(current_kind);
        prior_fact = Some(fact);
    }
    Ok(())
}

fn validate_required_check_policy(required_checks: &[RecoveryRequiredCheck]) -> RecoveryResult<()> {
    if required_checks.len() > MAX_FAILED_CONTEXTS {
        return Err(RecoveryError::InvalidRequest(format!(
            "required_checks must contain at most {MAX_FAILED_CONTEXTS} structured identities"
        )));
    }
    let mut prior_required = None;
    for (index, required) in required_checks.iter().enumerate() {
        validate_text(
            "required_check policy context",
            &required.context,
            1,
            MAX_FAILED_CONTEXT_BYTES,
        )?;
        if required.app_id == Some(0) {
            return Err(RecoveryError::InvalidRequest(
                "required-check policy app_id must be positive".to_owned(),
            ));
        }
        if prior_required.is_some_and(|prior| prior >= required) {
            return Err(RecoveryError::InvalidRequest(
                "required_checks must be strictly sorted and unique".to_owned(),
            ));
        }
        if required_checks[..index].iter().any(|prior| {
            prior.context.eq_ignore_ascii_case(&required.context) && prior.app_id == required.app_id
        }) {
            return Err(RecoveryError::InvalidRequest(
                "required_checks contain a case-insensitive duplicate identity".to_owned(),
            ));
        }
        prior_required = Some(required);
    }
    Ok(())
}

fn validate_base_ref(base_ref: &str) -> RecoveryResult<()> {
    validate_text("base_ref", base_ref, 1, MAX_BASE_REF_BYTES)?;
    if base_ref.starts_with('/')
        || base_ref.ends_with('/')
        || base_ref.ends_with('.')
        || base_ref.to_ascii_lowercase().ends_with(".lock")
        || base_ref.contains("//")
        || base_ref.contains("..")
        || base_ref.contains("@{")
        || base_ref
            .bytes()
            .any(|byte| byte.is_ascii_control() || b" ~^:?*[\\".contains(&byte))
    {
        return Err(RecoveryError::InvalidRequest(
            "base_ref must be a bounded GitHub branch name".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_repo(repo: &str) -> RecoveryResult<()> {
    validate_text("repo", repo, 1, MAX_REPO_BYTES)?;
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(RecoveryError::InvalidRequest(
            "repo must be an owner/repository slug".to_owned(),
        ));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || !owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(RecoveryError::InvalidRequest(
            "repo must be an owner/repository slug".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn validate_signature(name: &str, value: &str) -> RecoveryResult<()> {
    validate_text(name, value, 1, MAX_SIGNATURE_BYTES)
}

pub(super) fn validate_id(id: &str) -> RecoveryResult<()> {
    if id.len() == 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(RecoveryError::InvalidRequest(
            "recovery id must be a full 64-character hexadecimal SHA-256".to_owned(),
        ))
    }
}

pub(super) fn validate_text(name: &str, value: &str, min: usize, max: usize) -> RecoveryResult<()> {
    let length = value.len();
    if length < min || length > max || value.trim().is_empty() || value.contains('\0') {
        return Err(RecoveryError::InvalidRequest(format!(
            "{name} must be {min}..={max} bytes and contain no NUL"
        )));
    }
    Ok(())
}

pub(crate) fn validate_record(record: &RecoveryRecord) -> RecoveryResult<()> {
    if record.schema_version != RECOVERY_SCHEMA_VERSION {
        return Err(RecoveryError::SchemaVersion {
            surface: "record",
            observed: record.schema_version,
        });
    }
    validate_request(&record.request)?;
    if record.receipt.schema_version != RECOVERY_SCHEMA_VERSION {
        return Err(RecoveryError::SchemaVersion {
            surface: "receipt",
            observed: record.receipt.schema_version,
        });
    }
    if record.receipt.request_id != record.request.id {
        return Err(RecoveryError::IdentityCollision(record.request.id.clone()));
    }
    if record.receipt.config_signature != record.request.config_signature {
        return Err(RecoveryError::ConfigDrift {
            expected: record.request.config_signature.clone(),
            observed: record.receipt.config_signature.clone(),
        });
    }
    if record.receipt.max_attempts == 0 || record.receipt.attempt > record.receipt.max_attempts {
        return Err(RecoveryError::InvalidRequest(
            "receipt attempt budget is invalid".to_owned(),
        ));
    }
    if let Some(generation) = record.receipt.worker_generation.as_deref() {
        validate_text("worker_generation", generation, 1, MAX_GENERATION_BYTES)?;
    }
    if let Some(successor) = record.receipt.superseded_by.as_deref() {
        validate_id(successor)?;
    }
    if let Some(detail) = record.receipt.detail.as_deref() {
        validate_text("receipt detail", detail, 1, MAX_DETAIL_BYTES)?;
    }
    if let Some(output) = record.receipt.output.as_ref() {
        output.validate()?;
    }
    match record.receipt.status {
        RecoveryStatus::Pending => validate_pending_receipt(record)?,
        RecoveryStatus::Running => {
            if record.receipt.attempt == 0
                || record.receipt.worker_generation.is_none()
                || record.receipt.started_at.is_none()
                || record.receipt.completed_at.is_some()
                || record.receipt.deferred_at.is_some()
                || record.receipt.output.is_some()
            {
                return Err(RecoveryError::InvalidRequest(
                    "running receipt lacks exact worker state".to_owned(),
                ));
            }
        }
        RecoveryStatus::Triaged | RecoveryStatus::Escalated => {
            if record.receipt.completed_at.is_none()
                || record.receipt.deferred_at.is_some()
                || record.receipt.output.is_none()
            {
                return Err(RecoveryError::InvalidRequest(
                    "completed recovery receipt lacks structured output".to_owned(),
                ));
            }
        }
        RecoveryStatus::Superseded => {
            if record.receipt.completed_at.is_none()
                || record.receipt.deferred_at.is_some()
                || record.receipt.detail.is_none()
                || record.receipt.output.is_some()
            {
                return Err(RecoveryError::InvalidRequest(
                    "superseded receipt lacks successor or detail".to_owned(),
                ));
            }
        }
        RecoveryStatus::Failed => {
            if record.receipt.completed_at.is_none()
                || record.receipt.deferred_at.is_some()
                || record.receipt.detail.is_none()
                || record.receipt.output.is_some()
            {
                return Err(RecoveryError::InvalidRequest(
                    "failed receipt lacks terminal detail".to_owned(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_pending_receipt(record: &RecoveryRecord) -> RecoveryResult<()> {
    if record.receipt.attempt != 0
        || record.receipt.worker_generation.is_some()
        || record.receipt.started_at.is_some()
        || record.receipt.completed_at.is_some()
        || record.receipt.output.is_some()
        || record.receipt.deferred_at.is_some() != record.receipt.detail.is_some()
        || record
            .receipt
            .deferred_at
            .is_some_and(|deferred_at| deferred_at != record.receipt.updated_at)
    {
        return Err(RecoveryError::InvalidRequest(
            "pending receipt contains worker or terminal state".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn same_recovery_identity(left: &RecoveryRequest, right: &RecoveryRequest) -> bool {
    left.repo == right.repo
        && left.pr == right.pr
        && left.base_ref == right.base_ref
        && left.head_sha == right.head_sha
        && left.merge_queue == right.merge_queue
        && left.opt_out_label == right.opt_out_label
        && left.failure_fingerprint == right.failure_fingerprint
        && left.failure_summary == right.failure_summary
        && left.required_checks == right.required_checks
        && left.failure_facts == right.failure_facts
        && left.policy_signature == right.policy_signature
}

pub(super) fn ensure_config(record: &RecoveryRecord, observed: &str) -> RecoveryResult<()> {
    if record.request.config_signature == observed && record.receipt.config_signature == observed {
        Ok(())
    } else {
        Err(RecoveryError::ConfigDrift {
            expected: record.request.config_signature.clone(),
            observed: observed.to_owned(),
        })
    }
}

pub(super) fn invalid_output(message: impl Into<String>) -> RecoveryError {
    RecoveryError::InvalidOutput(message.into())
}

pub(super) fn invalid_transition(
    id: &str,
    status: RecoveryStatus,
    requested: &'static str,
) -> RecoveryError {
    RecoveryError::InvalidTransition {
        id: id.to_owned(),
        status,
        requested,
    }
}
