use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;
use toml::{Table, Value as TomlValue};

use super::{CliFailure, cli::ConfigCommand};
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::output::{write_json_envelope, write_pretty_json};

pub(super) fn config_command<W: Write>(
    command: Option<ConfigCommand>,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    match command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => config_show(&config, json, stdout)?,
        ConfigCommand::Profiles => config_profiles(&config, json, stdout)?,
        ConfigCommand::Use { profile_name } => config_use(&config, &profile_name, json, stdout)?,
    }
    Ok(ExitCode::SUCCESS)
}

fn config_show<W: Write>(
    config: &LoadedConfig,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "config".to_owned(),
            serde_json::to_value(&config.data)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "config.show", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        write_pretty_json(stdout, &config.data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn config_profiles<W: Write>(
    config: &LoadedConfig,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let rows = profile_rows(config);
    let active = active_profile(config);
    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "profiles".to_owned(),
            serde_json::to_value(&rows).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "active".to_owned(),
            active
                .as_ref()
                .map_or(Value::Null, |profile| Value::String(profile.clone())),
        );
        write_json_envelope(stdout, "config.profiles", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    if rows.is_empty() {
        writeln!(stdout, "No profiles defined. See docs/profiles.md.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "Profiles").map_err(|error| CliFailure::new(1, error.to_string()))?;
    for row in rows {
        let marker = if row.active { " <- active" } else { "" };
        writeln!(
            stdout,
            "  {:<10} {}{}{}",
            row.name,
            display_list(&row.targets),
            display_profile_policy(&row),
            marker
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    writeln!(stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(())
}

fn config_use<W: Write>(
    config: &LoadedConfig,
    profile_name: &str,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let project_dir = config.project_dir.as_ref().ok_or_else(|| {
        CliFailure::new(
            1,
            "No .shipyard/config.toml found. Run `shipyard init` first.",
        )
    })?;
    if !profile_names(config)
        .iter()
        .any(|name| name == profile_name)
    {
        let known = profile_names(config);
        return Err(CliFailure::new(
            1,
            format!(
                "Profile '{profile_name}' is not defined. Known profiles: {}",
                if known.is_empty() {
                    "(none)".to_owned()
                } else {
                    known.join(", ")
                }
            ),
        ));
    }

    let config_path = project_dir.join("config.toml");
    rewrite_profile_in_config(&config_path, profile_name)?;
    if json {
        let mut data = BTreeMap::new();
        data.insert("profile".to_owned(), Value::String(profile_name.to_owned()));
        write_json_envelope(stdout, "config.use", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "Switched to profile '{profile_name}' in {}",
            config_path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq, Serialize)]
struct ProfileRow {
    name: String,
    active: bool,
    targets: Vec<String>,
    description: Option<String>,
    focus_platforms: Vec<String>,
    advisory_platforms: Vec<String>,
}

fn profile_rows(config: &LoadedConfig) -> Vec<ProfileRow> {
    let active = active_profile(config);
    let Some(profiles) = config.get("profiles").and_then(TomlValue::as_table) else {
        return Vec::new();
    };
    profiles
        .iter()
        .map(|(name, body)| ProfileRow {
            name: name.clone(),
            active: active.as_deref() == Some(name.as_str()),
            targets: profile_targets(body),
            description: profile_string(body, "description"),
            focus_platforms: profile_string_array(body, "focus_platforms"),
            advisory_platforms: profile_string_array(body, "advisory_platforms"),
        })
        .collect()
}

fn profile_names(config: &LoadedConfig) -> Vec<String> {
    config
        .get("profiles")
        .and_then(TomlValue::as_table)
        .map(|profiles| profiles.keys().cloned().collect())
        .unwrap_or_default()
}

fn profile_targets(value: &TomlValue) -> Vec<String> {
    profile_string_array(value, "targets")
}

fn profile_string_array(value: &TomlValue, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(TomlValue::as_array)
        .map(|targets| {
            targets
                .iter()
                .filter_map(TomlValue::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn profile_string(value: &TomlValue, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(TomlValue::as_str)
        .map(ToOwned::to_owned)
}

fn display_list(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_owned()
    } else {
        items.join(", ")
    }
}

fn display_profile_policy(row: &ProfileRow) -> String {
    let mut parts = Vec::new();
    if !row.focus_platforms.is_empty() {
        parts.push(format!("focus={}", row.focus_platforms.join(",")));
    }
    if !row.advisory_platforms.is_empty() {
        parts.push(format!("advisory={}", row.advisory_platforms.join(",")));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join("; "))
    }
}

fn active_profile(config: &LoadedConfig) -> Option<String> {
    config
        .get_str("project.profile")
        .filter(|profile| !profile.is_empty())
        .map(ToOwned::to_owned)
}

fn rewrite_profile_in_config(config_path: &Path, profile_name: &str) -> Result<(), CliFailure> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(config_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let contents =
        fs::read_to_string(config_path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut table = contents
        .parse::<Table>()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let project = table
        .entry("project".to_owned())
        .or_insert_with(|| TomlValue::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| CliFailure::new(1, "`project` config section must be a table"))?;
    project.insert(
        "profile".to_owned(),
        TomlValue::String(profile_name.to_owned()),
    );
    fs::write(config_path, format!("{table}\n"))
        .map_err(|error| CliFailure::new(1, error.to_string()))
}
