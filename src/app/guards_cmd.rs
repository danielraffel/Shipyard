//! `shipyard guards` — install and audit the `ghapp` wrapper's queue guards.
//!
//! See [`crate::ghapp_guards`] for what is managed and why.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use super::CliFailure;
use super::cli::GuardsCommand;
use crate::ghapp_guards::{
    FLEET_NOTE, GuardState, SwitchState, audit, audit_review_switch, default_guards_dir, install,
    opt_in_review, opt_out_review,
};
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
            let switch = audit_review_switch(&dir);
            if json {
                let mut data = BTreeMap::new();
                data.insert("dir".to_owned(), serde_json::json!(dir));
                data.insert("guards".to_owned(), serde_json::json!(rows));
                data.insert("switches".to_owned(), serde_json::json!([switch]));
                data.insert("note".to_owned(), serde_json::json!(FLEET_NOTE));
                write_json_envelope(stdout, "guards status", data)
                    .map_err(|error| CliFailure::new(1, error.to_string()))?;
            } else {
                writeln!(stdout, "ghapp guards in {}", dir.display()).map_err(io)?;
                for row in &rows {
                    writeln!(stdout, "  {:<22} {}", row.name, describe(&row.state)).map_err(io)?;
                }
                writeln!(
                    stdout,
                    "  {:<22} {}",
                    switch.name,
                    describe_switch(&switch.state)
                )
                .map_err(io)?;
                writeln!(stdout, "{FLEET_NOTE}").map_err(io)?;
            }
            let switch_ok = matches!(
                switch.state,
                SwitchState::Current | SwitchState::OptedOut { .. }
            );
            if rows.iter().all(|row| row.state == GuardState::Current) && switch_ok {
                Ok(ExitCode::SUCCESS)
            } else {
                Err(CliFailure::new(
                    1,
                    "one or more ghapp guards are missing or stale, or review-before-arm is off \
                     without an opt-out; run `shipyard guards install`",
                ))
            }
        }
        GuardsCommand::Install {
            dir,
            dry_run,
            opt_out_review: opt_out_reason,
            opt_in_review: opt_in,
        } => {
            let dir: PathBuf = dir.unwrap_or_else(default_guards_dir);
            if let Some(reason) = opt_out_reason.as_deref() {
                opt_out_review(&dir, reason).map_err(|error| CliFailure::new(2, error))?;
                eprintln!(
                    "WARNING: review-before-arm is OFF on this host ({reason}); the arm guard \
                     will not check reviewer verdicts. Logged to {}.",
                    dir.join(crate::ghapp_guards::SWITCH_LOG).display()
                );
            } else if opt_in {
                opt_in_review(&dir).map_err(|error| CliFailure::new(1, error))?;
            }
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

fn describe_switch(state: &SwitchState) -> String {
    match state {
        SwitchState::Current => "on".to_owned(),
        SwitchState::Missing => "MISSING (review-before-arm is off without an opt-out)".to_owned(),
        SwitchState::OptedOut { record } => format!("OPTED OUT {record}"),
        SwitchState::Unreadable { detail } => format!("UNREADABLE ({detail})"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn status(dir: &std::path::Path) -> Result<ExitCode, CliFailure> {
        guards_command(
            GuardsCommand::Status {
                dir: Some(dir.to_path_buf()),
            },
            true,
            &mut Vec::new(),
        )
    }

    fn install_cmd(
        dir: &std::path::Path,
        opt_out: Option<&str>,
        opt_in: bool,
    ) -> Result<ExitCode, CliFailure> {
        guards_command(
            GuardsCommand::Install {
                dir: Some(dir.to_path_buf()),
                dry_run: false,
                opt_out_review: opt_out.map(str::to_owned),
                opt_in_review: opt_in,
            },
            true,
            &mut Vec::new(),
        )
    }

    /// Status fails on a host where review-before-arm is off by accident, and
    /// passes when it is on or explicitly opted out.
    #[test]
    fn status_reports_a_missing_review_switch_as_not_current() {
        let temp = tempfile::tempdir().expect("temp");
        let dir = temp.path();
        install_cmd(dir, None, false).expect("install");
        assert!(status(dir).is_ok(), "control: a fresh install is current");
        std::fs::remove_file(dir.join(crate::ghapp_guards::REVIEW_SWITCH)).expect("remove");
        assert!(status(dir).is_err(), "a missing switch is not current");
        install_cmd(dir, Some("lab host"), false).expect("opt out");
        assert!(status(dir).is_ok(), "an explicit opt-out is current");
        let mut out = Vec::new();
        let _ = guards_command(
            GuardsCommand::Status {
                dir: Some(dir.to_path_buf()),
            },
            true,
            &mut out,
        );
        let text = String::from_utf8(out).expect("utf8");
        assert!(text.contains("\"switches\""), "{text}");
        assert!(text.contains("opted_out"), "{text}");
        install_cmd(dir, None, true).expect("opt in");
        assert!(dir.join(crate::ghapp_guards::REVIEW_SWITCH).is_file());
        assert!(status(dir).is_ok());
    }
}
