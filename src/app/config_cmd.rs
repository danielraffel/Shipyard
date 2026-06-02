use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;
use toml::{Table, Value as TomlValue};

use super::{CliFailure, cli::ConfigCommand};
use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::identity::{ProductIdentity, RuntimeMode};
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
        ConfigCommand::Use {
            profile_name,
            local,
        } => config_use(&config, mode, cwd, &profile_name, local, json, stdout)?,
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
    if json {
        let provenance = resolve_active_profile_provenance(config);
        let mut data = BTreeMap::new();
        data.insert(
            "profiles".to_owned(),
            serde_json::to_value(&rows).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        data.insert(
            "active".to_owned(),
            provenance
                .profile
                .as_ref()
                .map_or(Value::Null, |profile| Value::String(profile.clone())),
        );
        data.insert(
            "active_source".to_owned(),
            Value::String(provenance.source.to_owned()),
        );
        data.insert("active_path".to_owned(), path_value(provenance.path.as_ref()));
        data.insert(
            "local_overlay_source".to_owned(),
            Value::String(local_overlay_source_str(config.local_overlay_source).to_owned()),
        );
        data.insert(
            "local_overlay_path".to_owned(),
            path_value(config.local_dir.as_ref().map(|d| d.join("config.toml")).as_ref()),
        );
        data.insert(
            "tracked_config_path".to_owned(),
            path_value(
                config
                    .project_dir
                    .as_ref()
                    .map(|d| d.join("config.toml"))
                    .as_ref(),
            ),
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
    mode: RuntimeMode,
    cwd: &Path,
    profile_name: &str,
    local: bool,
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

    // `--local` writes the per-machine overlay for THIS checkout, never the
    // committed tracked config and never a borrowed worktree-fallback overlay.
    let (config_path, scope) = if local {
        (local_overlay_config_path(mode, cwd, config), "local")
    } else {
        (project_dir.join("config.toml"), "tracked")
    };
    upsert_profile_in_config(&config_path, profile_name)?;
    if json {
        let mut data = BTreeMap::new();
        data.insert("profile".to_owned(), Value::String(profile_name.to_owned()));
        data.insert("scope".to_owned(), Value::String(scope.to_owned()));
        data.insert(
            "path".to_owned(),
            Value::String(config_path.display().to_string()),
        );
        data.insert(
            "active_source".to_owned(),
            Value::String(scope.to_owned()),
        );
        write_json_envelope(stdout, "config.use", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "Switched to profile '{profile_name}' ({scope}) in {}",
            config_path.display()
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

/// Resolve the per-machine local-overlay `config.toml` write path for this
/// checkout. Mirrors `auth_cmd::config_path_for_scope`'s Local arm: use the
/// directly-resolved overlay when present, otherwise the current checkout's own
/// overlay dir — never a borrowed worktree-fallback overlay.
fn local_overlay_config_path(mode: RuntimeMode, cwd: &Path, config: &LoadedConfig) -> PathBuf {
    let identity = ProductIdentity::for_mode(mode);
    let dir = match config.local_overlay_source {
        LocalOverlaySource::Direct => config
            .local_dir
            .clone()
            .unwrap_or_else(|| cwd.join(identity.local_overlay_dir_name)),
        LocalOverlaySource::WorktreeFallback | LocalOverlaySource::None => {
            cwd.join(identity.local_overlay_dir_name)
        }
    };
    dir.join("config.toml")
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

struct ActiveProvenance {
    profile: Option<String>,
    source: &'static str,
    path: Option<PathBuf>,
}

/// Report the effective active profile (merged, non-empty) AND which config layer
/// supplied it, by re-reading each layer's raw `[project].profile`. Precedence
/// matches the merge order: local overlay > tracked project > global. Keeping the
/// reported `active` equal to `active_profile(config)` ensures CLI output never
/// disagrees with effective runtime config.
fn resolve_active_profile_provenance(config: &LoadedConfig) -> ActiveProvenance {
    let Some(active) = active_profile(config) else {
        return ActiveProvenance {
            profile: None,
            source: "none",
            path: None,
        };
    };
    let layers: [(Option<PathBuf>, &'static str); 3] = [
        (
            config.local_dir.as_ref().map(|d| d.join("config.toml")),
            "local",
        ),
        (
            config.project_dir.as_ref().map(|d| d.join("config.toml")),
            "tracked",
        ),
        (Some(config.global_dir.join("config.toml")), "global"),
    ];
    for (path, source) in layers {
        if let Some(path) = path
            && raw_project_profile(&path).as_deref() == Some(active.as_str())
        {
            return ActiveProvenance {
                profile: Some(active),
                source,
                path: Some(path),
            };
        }
    }
    // Active is set in merged data but no raw layer matched (unexpected).
    ActiveProvenance {
        profile: Some(active),
        source: "unknown",
        path: None,
    }
}

/// Read the raw, non-empty `[project].profile` string from a single config file.
fn raw_project_profile(path: &Path) -> Option<String> {
    let table = fs::read_to_string(path).ok()?.parse::<Table>().ok()?;
    table
        .get("project")?
        .as_table()?
        .get("profile")?
        .as_str()
        .filter(|profile| !profile.is_empty())
        .map(ToOwned::to_owned)
}

fn local_overlay_source_str(source: LocalOverlaySource) -> &'static str {
    match source {
        LocalOverlaySource::Direct => "direct",
        LocalOverlaySource::WorktreeFallback => "worktree_fallback",
        LocalOverlaySource::None => "none",
    }
}

fn path_value(path: Option<&PathBuf>) -> Value {
    path.map_or(Value::Null, |p| Value::String(p.display().to_string()))
}

/// Create-or-update: set `[project].profile` in the TOML at `config_path`,
/// creating the file and its parent dir when absent (the local-overlay case).
fn upsert_profile_in_config(config_path: &Path, profile_name: &str) -> Result<(), CliFailure> {
    let mut table = if config_path.exists() {
        fs::read_to_string(config_path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
            .parse::<Table>()
            .map_err(|error| CliFailure::new(1, error.to_string()))?
    } else {
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        Table::new()
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use toml::Table;

    fn config_with(local_dir: Option<PathBuf>, source: LocalOverlaySource) -> LoadedConfig {
        LoadedConfig {
            data: Table::new(),
            global_dir: PathBuf::from("/global"),
            project_dir: Some(PathBuf::from("/repo/.shipyard")),
            local_dir,
            local_overlay_source: source,
        }
    }

    #[test]
    fn local_write_uses_direct_overlay_when_present() {
        let config = config_with(
            Some(PathBuf::from("/repo/.shipyard.local")),
            LocalOverlaySource::Direct,
        );
        let path = local_overlay_config_path(RuntimeMode::Shipyard, Path::new("/repo"), &config);
        assert_eq!(path, PathBuf::from("/repo/.shipyard.local/config.toml"));
    }

    #[test]
    fn local_write_targets_current_checkout_not_borrowed_worktree_fallback() {
        // `local_dir` points at a BORROWED main-checkout overlay (worktree
        // fallback). A `--local` write must target the CURRENT checkout's own
        // overlay, never the borrowed one.
        let config = config_with(
            Some(PathBuf::from("/main/.shipyard.local")),
            LocalOverlaySource::WorktreeFallback,
        );
        let path =
            local_overlay_config_path(RuntimeMode::Shipyard, Path::new("/worktree"), &config);
        assert_eq!(path, PathBuf::from("/worktree/.shipyard.local/config.toml"));
    }

    #[test]
    fn local_write_falls_back_to_cwd_when_no_overlay() {
        let config = config_with(None, LocalOverlaySource::None);
        let path = local_overlay_config_path(RuntimeMode::Shipyard, Path::new("/repo"), &config);
        assert_eq!(path, PathBuf::from("/repo/.shipyard.local/config.toml"));
    }
}
