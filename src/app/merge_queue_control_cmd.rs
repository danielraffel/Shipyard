use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use super::CliFailure;
use super::cli::MergeQueueCommand;
use crate::identity::RuntimeMode;
use crate::merge_queue_control::{
    HOLD_FILE, authority_status, hold, hold_status, resolve_uncertainty, resume,
    uncertain_mutations,
};
use crate::output::write_json_envelope;

pub(super) fn merge_queue_control_command<W: Write>(
    command: MergeQueueCommand,
    state_root: &Path,
    global_dir: &Path,
    cwd: &Path,
    mode: RuntimeMode,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        MergeQueueCommand::Status => {
            let status = hold_status(state_root).map_err(|error| CliFailure::new(1, error))?;
            let authority = authority_status(state_root, cwd, mode, global_dir)
                .map_err(|error| CliFailure::new(1, error))?;
            if json {
                let mut data = BTreeMap::new();
                data.insert("held".to_owned(), Value::Bool(status.is_some()));
                data.insert(
                    "path".to_owned(),
                    Value::String(state_root.join(HOLD_FILE).display().to_string()),
                );
                if let Some(status) = status {
                    data.insert("hold".to_owned(), status);
                }
                data.insert(
                    "uncertain_mutations".to_owned(),
                    Value::Array(
                        uncertain_mutations(state_root)
                            .map_err(|error| CliFailure::new(1, error))?,
                    ),
                );
                let authority = authority
                    .as_object()
                    .expect("authority status is an object");
                data.extend(authority.clone());
                write_json_envelope(stdout, "merge-queue.status", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else if let Some(status) = status {
                writeln!(
                    stdout,
                    "held: {}",
                    status["reason"].as_str().unwrap_or("unspecified")
                )
                .ok();
            } else {
                writeln!(stdout, "active").ok();
            }
            if !json {
                writeln!(
                    stdout,
                    "authority: machine={} configured={} matches={}",
                    authority["machine"].as_str().unwrap_or("unconfigured"),
                    authority["mutation_machine"]
                        .as_str()
                        .unwrap_or("unconfigured"),
                    authority["authority_matches"].as_bool().unwrap_or(false)
                )
                .ok();
            }
        }
        MergeQueueCommand::Hold { reason } => {
            if reason.trim().is_empty() {
                return Err(CliFailure::new(2, "--reason must not be empty"));
            }
            let path =
                hold(state_root, reason.trim()).map_err(|error| CliFailure::new(1, error))?;
            if json {
                let mut data = BTreeMap::new();
                data.insert("held".to_owned(), Value::Bool(true));
                data.insert("path".to_owned(), Value::String(path.display().to_string()));
                write_json_envelope(stdout, "merge-queue.hold", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else {
                writeln!(stdout, "merge-queue mutations held ({})", path.display()).ok();
            }
        }
        MergeQueueCommand::Resume => {
            let removed = resume(state_root).map_err(|error| CliFailure::new(1, error))?;
            if json {
                let mut data = BTreeMap::new();
                data.insert("held".to_owned(), Value::Bool(false));
                data.insert("changed".to_owned(), Value::Bool(removed));
                write_json_envelope(stdout, "merge-queue.resume", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else if removed {
                writeln!(stdout, "merge-queue mutation hold removed").ok();
            } else {
                writeln!(stdout, "merge-queue mutations were already active").ok();
            }
        }
        MergeQueueCommand::Resolve {
            correlation_id,
            outcome,
            reason,
        } => resolve_command(state_root, &correlation_id, &outcome, &reason, json, stdout)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn resolve_command<W: Write>(
    state_root: &Path,
    correlation_id: &str,
    outcome: &str,
    reason: &str,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if reason.trim().is_empty() {
        return Err(CliFailure::new(2, "--reason must not be empty"));
    }
    resolve_uncertainty(
        state_root,
        correlation_id.trim(),
        outcome.trim(),
        reason.trim(),
    )
    .map_err(|error| CliFailure::new(1, error))?;
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "correlation_id".to_owned(),
            Value::String(correlation_id.to_owned()),
        );
        data.insert("outcome".to_owned(), Value::String(outcome.to_owned()));
        write_json_envelope(stdout, "merge-queue.resolve", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "merge-queue uncertainty resolved").ok();
    }
    Ok(())
}
