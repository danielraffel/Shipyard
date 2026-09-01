//! CLI boundary for the default-off custody setup/doctor contract.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::custody_transport::{
    CustodySetupReport, custody_disable, custody_doctor, custody_provision,
};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

use super::CliFailure;
use super::cli::CustodyCommand;

/// Run `shipyard custody ...` against the machine-global policy only.
pub(super) fn custody_command<W: Write>(
    command: CustodyCommand,
    mode: RuntimeMode,
    global_dir: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if mode != RuntimeMode::Shipyard {
        return Err(CliFailure::new(
            1,
            "custody setup is available only in production Shipyard mode; isolated mode cannot grant cross-machine custody",
        ));
    }
    let report = match command {
        CustodyCommand::Doctor => custody_doctor(global_dir),
        CustodyCommand::Provision { input, apply } => custody_provision(global_dir, &input, apply),
        CustodyCommand::Disable {
            policy_digest,
            apply,
        } => custody_disable(global_dir, state_dir, &policy_digest, apply),
    };
    render_report(&report, json, stdout)?;
    Ok(ExitCode::from(u8::from(!report.ready)))
}

fn render_report<W: Write>(
    report: &CustodySetupReport,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let value =
            serde_json::to_value(report).map_err(|error| CliFailure::new(1, error.to_string()))?;
        let Value::Object(map) = value else {
            return Err(CliFailure::new(
                1,
                "custody report must serialize as an object",
            ));
        };
        let data = map.into_iter().collect();
        write_json_envelope(stdout, "custody.setup", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout, "shipyard custody: {}", report.outcome)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "ready: {}", if report.ready { "yes" } else { "no" })
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if let Some(machine) = report.local_machine_ref.as_deref() {
        writeln!(stdout, "local machine: {machine}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if let Some(digest) = report.policy_digest.as_deref() {
        writeln!(stdout, "policy digest: {digest}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    if !report.paths.is_empty() {
        writeln!(stdout, "paths:").map_err(|error| CliFailure::new(1, error.to_string()))?;
        for path in &report.paths {
            writeln!(stdout, "  {path}").map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }
    if let Some(reason) = report.reason_code.as_deref() {
        writeln!(stdout, "reason: {reason}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    for check in &report.checks {
        writeln!(
            stdout,
            "  {}: {} ({})",
            check.name,
            if check.ok { "ok" } else { "fail" },
            check.detail
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}
