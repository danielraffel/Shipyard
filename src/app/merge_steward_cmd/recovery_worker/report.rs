use super::{
    BTreeMap, CliFailure, RecoveryRecord, RecoveryRequest, RecoveryWorkerPolicy,
    RecoveryWorkerReport, Value, Write,
};

pub(super) fn report(
    request: &RecoveryRequest,
    action: &str,
    detail: impl Into<String>,
) -> RecoveryWorkerReport {
    RecoveryWorkerReport {
        request_id: request.id.clone(),
        repo: request.repo.clone(),
        pr: request.pr,
        head_sha: request.head_sha.clone(),
        action: action.to_owned(),
        detail: detail.into(),
    }
}

pub(super) fn report_error(record: &RecoveryRecord, detail: &str) -> RecoveryWorkerReport {
    report(&record.request, "error", detail.to_owned())
}

pub(super) fn render_reports<W: Write>(
    stdout: &mut W,
    json: bool,
    apply: bool,
    policy: &RecoveryWorkerPolicy,
    policy_signature: &str,
    reports: &[RecoveryWorkerReport],
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("apply".to_owned(), Value::from(apply));
        data.insert("provider".to_owned(), Value::from(policy.provider.clone()));
        data.insert(
            "model".to_owned(),
            Value::from(policy.first_line_model.clone()),
        );
        data.insert(
            "policy_signature".to_owned(),
            Value::from(policy_signature.to_owned()),
        );
        data.insert(
            "requests".to_owned(),
            serde_json::to_value(reports).map_err(|error| {
                CliFailure::new(1, format!("failed to render recovery report: {error}"))
            })?,
        );
        crate::output::write_json_envelope(stdout, "runner:recovery-worker", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if reports.is_empty() {
        writeln!(stdout, "No pending recovery requests.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "Recovery worker: {} request(s), {} mode, provider={}, model={}",
            reports.len(),
            if apply { "apply" } else { "dry-run" },
            policy.provider,
            policy.first_line_model
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        for item in reports {
            writeln!(
                stdout,
                "  {}#{} {} {} -- {}",
                item.repo,
                item.pr,
                &item.head_sha[..item.head_sha.len().min(12)],
                item.action,
                item.detail
            )
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    Ok(())
}
