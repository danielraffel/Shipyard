//! `shipyard guards` — install and audit the `ghapp` wrapper's queue guards.
//!
//! See [`crate::ghapp_guards`] for what is managed and why.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use super::CliFailure;
use super::cli::GuardsCommand;
use crate::ghapp_guards::{FLEET_NOTE, GuardState, audit, default_guards_dir, install};
use crate::output::write_json_envelope;

pub(super) fn guards_command<W: Write>(
    command: GuardsCommand,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let io = |error: std::io::Error| CliFailure::new(1, error.to_string());
    match command {
        GuardsCommand::Status { dir } => {
            let dir = dir.unwrap_or_else(default_guards_dir);
            let rows = audit(&dir);
            if json {
                let mut data = BTreeMap::new();
                data.insert("dir".to_owned(), serde_json::json!(dir));
                data.insert("guards".to_owned(), serde_json::json!(rows));
                data.insert("note".to_owned(), serde_json::json!(FLEET_NOTE));
                write_json_envelope(stdout, "guards status", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else {
                writeln!(stdout, "ghapp guards in {}", dir.display()).map_err(io)?;
                for row in &rows {
                    writeln!(stdout, "  {:<22} {}", row.name, describe(&row.state)).map_err(io)?;
                }
                writeln!(stdout, "{FLEET_NOTE}").map_err(io)?;
            }
            if rows.iter().all(|row| row.state == GuardState::Current) {
                Ok(ExitCode::SUCCESS)
            } else {
                Err(CliFailure::new(
                    1,
                    "one or more ghapp guards are missing or stale; run `shipyard guards install`",
                ))
            }
        }
        GuardsCommand::Install { dir, dry_run } => {
            let dir: PathBuf = dir.unwrap_or_else(default_guards_dir);
            let actions = install(&dir, dry_run).map_err(|error| CliFailure::new(1, error))?;
            if json {
                let mut data = BTreeMap::new();
                data.insert("dir".to_owned(), serde_json::json!(dir));
                data.insert("dry_run".to_owned(), serde_json::json!(dry_run));
                data.insert("guards".to_owned(), serde_json::json!(actions));
                data.insert("note".to_owned(), serde_json::json!(FLEET_NOTE));
                write_json_envelope(stdout, "guards install", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else {
                let verb = if dry_run {
                    "would install"
                } else {
                    "installing"
                };
                writeln!(stdout, "{verb} ghapp guards into {}", dir.display()).map_err(io)?;
                for action in &actions {
                    writeln!(
                        stdout,
                        "  {:<22} {}{}",
                        action.name,
                        action.action,
                        action
                            .detail
                            .as_deref()
                            .map_or_else(String::new, |detail| format!(" ({detail})"))
                    )
                    .map_err(io)?;
                }
                writeln!(stdout, "{FLEET_NOTE}").map_err(io)?;
            }
            if actions.iter().any(|action| action.action == "refused") {
                return Err(CliFailure::new(
                    1,
                    "refused to overwrite a guard that is not a regular file; inspect it by hand",
                ));
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn describe(state: &GuardState) -> String {
    match state {
        GuardState::Current => "current".to_owned(),
        GuardState::Missing => "MISSING (ghapp skips it)".to_owned(),
        GuardState::Stale { installed_sha256 } => {
            format!("STALE (installed sha256 {})", &installed_sha256[..12])
        }
        GuardState::NotExecutable => "NOT EXECUTABLE (ghapp skips it)".to_owned(),
        GuardState::Unmanaged { detail } => format!("UNMANAGED ({detail})"),
        GuardState::Unreadable { detail } => format!("UNREADABLE ({detail})"),
    }
}
