use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use toml::Value as TomlValue;

use super::{CliFailure, cli::QuarantineCommand};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;

const QUARANTINE_FILENAME: &str = "quarantine.toml";

pub(super) fn quarantine_command<W: Write>(
    command: Option<QuarantineCommand>,
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    match command.unwrap_or(QuarantineCommand::List) {
        QuarantineCommand::List => quarantine_list(&config, json_mode, stdout)?,
        QuarantineCommand::Add { target, reason } => {
            quarantine_add(&config, &target, &reason, json_mode, stdout)?;
        }
        QuarantineCommand::Remove { target } => {
            quarantine_remove(&config, &target, json_mode, stdout)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct QuarantineEntry {
    target: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    reason: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    added_at: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct QuarantineList {
    entries: Vec<QuarantineEntry>,
    path: Option<PathBuf>,
}

impl QuarantineList {
    fn load(path: Option<PathBuf>) -> Result<Self, CliFailure> {
        let Some(path) = path else {
            return Ok(Self {
                entries: Vec::new(),
                path: None,
            });
        };
        if !path.exists() {
            return Ok(Self {
                entries: Vec::new(),
                path: Some(path),
            });
        }
        let text =
            fs::read_to_string(&path).map_err(|error| CliFailure::new(1, error.to_string()))?;
        let parsed = text
            .parse::<toml::Table>()
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        let entries = parsed
            .get("quarantine")
            .and_then(TomlValue::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(TomlValue::as_table)
                    .filter_map(entry_from_table)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            entries,
            path: Some(path),
        })
    }

    fn add(&mut self, target: &str, reason: &str) -> bool {
        if self.entries.iter().any(|entry| entry.target == target) {
            return false;
        }
        self.entries.push(QuarantineEntry {
            target: target.to_owned(),
            reason: reason.trim().to_owned(),
            added_at: Utc::now().date_naive().to_string(),
        });
        true
    }

    fn remove(&mut self, target: &str) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.target != target);
        self.entries.len() < before
    }

    fn save(&self) -> Result<(), CliFailure> {
        let path = self
            .path
            .as_ref()
            .ok_or_else(|| CliFailure::new(1, "QuarantineList.save() requires a path"))?;
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        fs::write(path, render_quarantine(&self.entries))
            .map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

fn quarantine_list<W: Write>(
    config: &LoadedConfig,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let list = QuarantineList::load(quarantine_path(config))?;
    if json_mode {
        write_entries_json(stdout, "quarantine.list", &list.entries)?;
        return Ok(());
    }
    if list.entries.is_empty() {
        writeln!(
            stdout,
            "No quarantined targets. (.shipyard/quarantine.toml is absent or empty.)"
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Quarantined targets")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    for entry in list.entries {
        let reason = if entry.reason.is_empty() {
            String::new()
        } else {
            format!(" - {}", entry.reason)
        };
        let date = if entry.added_at.is_empty() {
            String::new()
        } else {
            format!(" (added {})", entry.added_at)
        };
        writeln!(stdout, "  {}{reason}{date}", entry.target)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn quarantine_add<W: Write>(
    config: &LoadedConfig,
    target: &str,
    reason: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let path = quarantine_path_or_error(config)?;
    let mut list = QuarantineList::load(Some(path.clone()))?;
    let added = list.add(target, reason);
    if added {
        list.save()?;
    }
    if json_mode {
        write_mutation_json(stdout, "quarantine.add", target, added, &path, "added")?;
    } else if added {
        writeln!(
            stdout,
            "Quarantined '{target}' - TEST/UNKNOWN failures on this target will be advisory."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "'{target}' was already quarantined.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn quarantine_remove<W: Write>(
    config: &LoadedConfig,
    target: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let path = quarantine_path_or_error(config)?;
    let mut list = QuarantineList::load(Some(path.clone()))?;
    let removed = list.remove(target);
    if removed {
        list.save()?;
    }
    if json_mode {
        write_mutation_json(
            stdout,
            "quarantine.remove",
            target,
            removed,
            &path,
            "removed",
        )?;
    } else if removed {
        writeln!(stdout, "Removed '{target}' from quarantine.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "'{target}' was not quarantined.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn quarantine_path(config: &LoadedConfig) -> Option<PathBuf> {
    config
        .project_dir
        .as_ref()
        .map(|dir| dir.join(QUARANTINE_FILENAME))
}

fn quarantine_path_or_error(config: &LoadedConfig) -> Result<PathBuf, CliFailure> {
    quarantine_path(config).ok_or_else(|| {
        CliFailure::new(
            1,
            "No .shipyard/ directory found. Run `shipyard init` first.",
        )
    })
}

fn entry_from_table(table: &toml::Table) -> Option<QuarantineEntry> {
    let target = table.get("target")?.as_str()?.trim().to_owned();
    if target.is_empty() {
        return None;
    }
    Some(QuarantineEntry {
        target,
        reason: table
            .get("reason")
            .and_then(TomlValue::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned(),
        added_at: table
            .get("added_at")
            .and_then(TomlValue::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    })
}

fn render_quarantine(entries: &[QuarantineEntry]) -> String {
    let mut text = String::new();
    for entry in entries {
        text.push_str("[[quarantine]]\n");
        writeln!(&mut text, "target = {}", toml_quote(&entry.target))
            .expect("writing to String cannot fail");
        if !entry.reason.is_empty() {
            writeln!(&mut text, "reason = {}", toml_quote(&entry.reason))
                .expect("writing to String cannot fail");
        }
        if !entry.added_at.is_empty() {
            writeln!(&mut text, "added_at = {}", toml_quote(&entry.added_at))
                .expect("writing to String cannot fail");
        }
        text.push('\n');
    }
    text
}

fn write_entries_json<W: Write>(
    stdout: &mut W,
    command: &str,
    entries: &[QuarantineEntry],
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert(
        "entries".to_owned(),
        serde_json::to_value(entries).map_err(|error| CliFailure::new(1, error.to_string()))?,
    );
    write_json_envelope(stdout, command, data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_mutation_json<W: Write>(
    stdout: &mut W,
    command: &str,
    target: &str,
    changed: bool,
    path: &Path,
    field: &str,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("target".to_owned(), Value::String(target.to_owned()));
    data.insert(field.to_owned(), Value::Bool(changed));
    data.insert("path".to_owned(), Value::String(path.display().to_string()));
    write_json_envelope(stdout, command, data)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn toml_quote(value: &str) -> String {
    serde_json::to_string(value).expect("strings serialize")
}
