use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde::Serialize;
use serde_json::Value;
use toml::{Table, Value as TomlValue};

use super::{
    CliFailure,
    cli::{ConfigBundleLayer, ConfigCommand, ConfigScope},
};
use crate::config::LoadedConfig;
use crate::identity::ProductIdentity;
use crate::identity::RuntimeMode;
use crate::machine_identity::existing_machine_id;
use crate::output::{write_json_envelope, write_pretty_json};

pub(super) fn config_command<W: Write>(
    command: Option<ConfigCommand>,
    mode: RuntimeMode,
    cwd: &Path,
    global_dir: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let config =
        LoadedConfig::load_from_cwd_with_global_dir(mode, cwd, Some(global_dir.to_path_buf()))
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    match command.unwrap_or(ConfigCommand::Show) {
        ConfigCommand::Show => config_show(&config, json, stdout)?,
        ConfigCommand::Profiles => config_profiles(&config, json, stdout)?,
        ConfigCommand::Use { profile_name } => config_use(&config, &profile_name, json, stdout)?,
        ConfigCommand::Set { key, value, scope } => config_set(
            ConfigWriteContext {
                mode,
                cwd,
                config: &config,
            },
            &key,
            &value,
            scope,
            json,
            stdout,
        )?,
        ConfigCommand::Unset { key, scope } => {
            config_unset(mode, cwd, &config, &key, scope, json, stdout)?;
        }
        ConfigCommand::Export {
            output,
            include_secrets,
        } => config_export(
            mode,
            state_dir,
            &config,
            output.as_deref(),
            include_secrets,
            json,
            stdout,
        )?,
        ConfigCommand::Import { input, from, scope } => config_import(
            ConfigWriteContext {
                mode,
                cwd,
                config: &config,
            },
            &input,
            from,
            scope,
            json,
            stdout,
        )?,
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

#[derive(Clone, Copy)]
struct ConfigWriteContext<'a> {
    mode: RuntimeMode,
    cwd: &'a Path,
    config: &'a LoadedConfig,
}

fn config_set<W: Write>(
    context: ConfigWriteContext<'_>,
    key: &str,
    value: &str,
    scope: ConfigScope,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let path = config_path_for_scope(context.mode, context.cwd, context.config, scope);
    let parsed_value = parse_config_value(value);
    mutate_config_file(&path, |table| set_dotted_value(table, key, parsed_value))?;
    write_config_mutation("config.set", &path, key, scope, json, stdout)
}

fn config_unset<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    config: &LoadedConfig,
    key: &str,
    scope: ConfigScope,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let path = config_path_for_scope(mode, cwd, config, scope);
    mutate_config_file(&path, |table| unset_dotted_value(table, key))?;
    write_config_mutation("config.unset", &path, key, scope, json, stdout)
}

fn config_export<W: Write>(
    mode: RuntimeMode,
    state_dir: &Path,
    config: &LoadedConfig,
    output: Option<&Path>,
    include_secrets: bool,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if include_secrets && output.is_none() {
        return Err(CliFailure::new(
            2,
            "config export --include-secrets requires --output to avoid writing secrets to stdout",
        ));
    }
    let bundle = export_setup_bundle(mode, state_dir, config, include_secrets)?;
    let text = format!("{bundle}\n");
    if let Some(path) = output {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        fs::write(path, text).map_err(|error| CliFailure::new(1, error.to_string()))?;
        #[cfg(unix)]
        if include_secrets {
            restrict_private_export_permissions(path)?;
        }
        if json {
            let mut data = BTreeMap::new();
            data.insert("path".to_owned(), Value::String(path.display().to_string()));
            data.insert("include_secrets".to_owned(), Value::Bool(include_secrets));
            if !include_secrets {
                data.insert(
                    "bundle".to_owned(),
                    serde_json::to_value(&bundle)
                        .map_err(|error| CliFailure::new(1, error.to_string()))?,
                );
            }
            return write_json_envelope(stdout, "config.export", data)
                .map_err(|error| CliFailure::new(1, error.to_string()));
        }
        writeln!(stdout, "Wrote {}", path.display())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }

    if json {
        let mut data = BTreeMap::new();
        data.insert(
            "bundle".to_owned(),
            serde_json::to_value(&bundle).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        write_json_envelope(stdout, "config.export", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        write!(stdout, "{text}").map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

#[cfg(unix)]
fn restrict_private_export_permissions(path: &Path) -> Result<(), CliFailure> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn config_import<W: Write>(
    context: ConfigWriteContext<'_>,
    input: &Path,
    from: ConfigBundleLayer,
    scope: ConfigScope,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let text = fs::read_to_string(input).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let bundle = text
        .parse::<Table>()
        .map_err(|error| CliFailure::new(1, format!("failed to parse setup bundle: {error}")))?;
    let imported = config_table_from_bundle(&bundle, from)?;
    let path = config_path_for_scope(context.mode, context.cwd, context.config, scope);
    write_config_file(&path, &imported)?;
    if json {
        let mut data = BTreeMap::new();
        data.insert("path".to_owned(), Value::String(path.display().to_string()));
        data.insert(
            "scope".to_owned(),
            Value::String(config_scope_label(scope).to_owned()),
        );
        data.insert(
            "from".to_owned(),
            Value::String(config_bundle_layer_label(from).to_owned()),
        );
        return write_json_envelope(stdout, "config.import", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "Imported {} config into {}",
        config_bundle_layer_label(from),
        path.display()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn config_path_for_scope(
    mode: RuntimeMode,
    cwd: &Path,
    config: &LoadedConfig,
    scope: ConfigScope,
) -> PathBuf {
    let identity = ProductIdentity::for_mode(mode);
    match scope {
        ConfigScope::Global => config.global_dir.join("config.toml"),
        ConfigScope::Project => config
            .project_dir
            .clone()
            .unwrap_or_else(|| cwd.join(identity.tracked_project_dir_name))
            .join("config.toml"),
        ConfigScope::Local => match config.local_overlay_source {
            crate::config::LocalOverlaySource::Direct => config
                .local_dir
                .clone()
                .unwrap_or_else(|| cwd.join(identity.local_overlay_dir_name))
                .join("config.toml"),
            crate::config::LocalOverlaySource::WorktreeFallback
            | crate::config::LocalOverlaySource::None => cwd
                .join(identity.local_overlay_dir_name)
                .join("config.toml"),
        },
    }
}

fn export_setup_bundle(
    mode: RuntimeMode,
    state_dir: &Path,
    config: &LoadedConfig,
    include_secrets: bool,
) -> Result<Table, CliFailure> {
    let mut metadata = Table::new();
    metadata.insert(
        "format".to_owned(),
        TomlValue::String("shipyard-setup".to_owned()),
    );
    metadata.insert("version".to_owned(), TomlValue::Integer(1));
    metadata.insert(
        "mode".to_owned(),
        TomlValue::String(
            match mode {
                RuntimeMode::Shipyard => "shipyard",
                RuntimeMode::Isolated => "isolated",
            }
            .to_owned(),
        ),
    );
    let machine_id = existing_machine_id(state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .filter(|value| !value.is_empty());
    if let Some(machine_id) = machine_id {
        metadata.insert("machine_id".to_owned(), TomlValue::String(machine_id));
    }

    let mut layers = Table::new();
    layers.insert(
        "global".to_owned(),
        TomlValue::Table(read_export_config_file(
            &config.global_dir.join("config.toml"),
            include_secrets,
        )?),
    );
    layers.insert(
        "project".to_owned(),
        TomlValue::Table(read_optional_export_config(
            config
                .project_dir
                .as_ref()
                .map(|dir| dir.join("config.toml")),
            include_secrets,
        )?),
    );
    layers.insert(
        "local".to_owned(),
        TomlValue::Table(read_optional_export_config(
            config.local_dir.as_ref().map(|dir| dir.join("config.toml")),
            include_secrets,
        )?),
    );
    layers.insert(
        "effective".to_owned(),
        TomlValue::Table(export_config_table(&config.data, include_secrets)),
    );

    let mut requirements = Table::new();
    let mut notes = Vec::new();
    if include_secrets {
        notes.push(TomlValue::String(
            "This bundle includes raw config secrets such as per-node bearer tokens; store it only in a private, trusted location.".to_owned(),
        ));
        notes.push(TomlValue::String(
            "Treat this file like a private key: keep permissions owner-only and never check it into a repository.".to_owned(),
        ));
        notes.push(TomlValue::String(
            "Token caches, daemon sockets, queue state, runtime logs, Keychain items, and 1Password sessions are still not exported.".to_owned(),
        ));
    } else {
        notes.push(TomlValue::String(
            "This bundle intentionally excludes raw tokens, private keys, token caches, daemon sockets, queue state, and runtime logs.".to_owned(),
        ));
        notes.push(TomlValue::String(
            "Use --include-secrets only for private backups when you need to preserve per-node pairing tokens or other raw config secrets.".to_owned(),
        ));
        notes.push(TomlValue::String(
            "Reprovision GitHub App private keys, Keychain items, 1Password sessions, and environment variables separately.".to_owned(),
        ));
    }
    requirements.insert("notes".to_owned(), TomlValue::Array(notes));

    let mut bundle = Table::new();
    bundle.insert("shipyard_setup".to_owned(), TomlValue::Table(metadata));
    bundle.insert("config".to_owned(), TomlValue::Table(layers));
    bundle.insert("requirements".to_owned(), TomlValue::Table(requirements));
    Ok(bundle)
}

fn read_optional_export_config(
    path: Option<PathBuf>,
    include_secrets: bool,
) -> Result<Table, CliFailure> {
    match path {
        Some(path) => read_export_config_file(&path, include_secrets),
        None => Ok(Table::new()),
    }
}

fn read_export_config_file(path: &Path, include_secrets: bool) -> Result<Table, CliFailure> {
    if !path.exists() {
        return Ok(Table::new());
    }
    let table = fs::read_to_string(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .parse::<Table>()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(export_config_table(&table, include_secrets))
}

fn export_config_table(table: &Table, include_secrets: bool) -> Table {
    table
        .iter()
        .filter_map(|(key, value)| {
            export_config_value(key, value, include_secrets).map(|value| (key.clone(), value))
        })
        .collect()
}

fn export_config_value(key: &str, value: &TomlValue, include_secrets: bool) -> Option<TomlValue> {
    if !include_secrets && is_secret_config_key(key) {
        return None;
    }
    match value {
        TomlValue::Table(table) => Some(TomlValue::Table(export_config_table(
            table,
            include_secrets,
        ))),
        TomlValue::Array(values) => Some(TomlValue::Array(
            values
                .iter()
                .map(|value| match value {
                    TomlValue::Table(table) => {
                        TomlValue::Table(export_config_table(table, include_secrets))
                    }
                    other => other.clone(),
                })
                .collect(),
        )),
        other => Some(other.clone()),
    }
}

fn is_secret_config_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "token"
            | "access_token"
            | "refresh_token"
            | "private_key"
            | "private_key_pem"
            | "pem"
            | "secret"
            | "client_secret"
            | "webhook_secret"
            | "bearer_token"
            | "node_token"
    ) || key.ends_with("_token")
        || key.ends_with("_secret")
        || key.ends_with("_key")
        || key.ends_with("_pem")
        || key.ends_with("_password")
        || key.ends_with("_credentials")
}

fn config_table_from_bundle(bundle: &Table, layer: ConfigBundleLayer) -> Result<Table, CliFailure> {
    if bundle
        .get("shipyard_setup")
        .and_then(TomlValue::as_table)
        .and_then(|metadata| metadata.get("version"))
        .and_then(TomlValue::as_integer)
        != Some(1)
    {
        return Err(CliFailure::new(
            1,
            "unsupported setup bundle version; expected [shipyard_setup] version = 1",
        ));
    }
    bundle
        .get("config")
        .and_then(TomlValue::as_table)
        .and_then(|config| config.get(config_bundle_layer_label(layer)))
        .and_then(TomlValue::as_table)
        .cloned()
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "setup bundle missing config.{}",
                    config_bundle_layer_label(layer)
                ),
            )
        })
}

fn write_config_mutation<W: Write>(
    command: &str,
    path: &Path,
    key: &str,
    scope: ConfigScope,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("path".to_owned(), Value::String(path.display().to_string()));
        data.insert("key".to_owned(), Value::String(key.to_owned()));
        data.insert(
            "scope".to_owned(),
            Value::String(config_scope_label(scope).to_owned()),
        );
        return write_json_envelope(stdout, command, data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "{} {key} in {}",
        if command == "config.set" {
            "Set"
        } else {
            "Unset"
        },
        path.display()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn config_scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Global => "global",
        ConfigScope::Project => "project",
        ConfigScope::Local => "local",
    }
}

fn config_bundle_layer_label(layer: ConfigBundleLayer) -> &'static str {
    match layer {
        ConfigBundleLayer::Global => "global",
        ConfigBundleLayer::Project => "project",
        ConfigBundleLayer::Local => "local",
        ConfigBundleLayer::Effective => "effective",
    }
}

fn mutate_config_file(
    path: &Path,
    mutate: impl FnOnce(&mut Table) -> Result<(), CliFailure>,
) -> Result<(), CliFailure> {
    let mut table = if path.exists() {
        fs::read_to_string(path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
            .parse::<Table>()
            .map_err(|error| CliFailure::new(1, error.to_string()))?
    } else {
        Table::new()
    };
    mutate(&mut table)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    fs::write(path, format!("{table}\n")).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn write_config_file(path: &Path, table: &Table) -> Result<(), CliFailure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    fs::write(path, format!("{table}\n")).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn parse_config_value(value: &str) -> TomlValue {
    format!("value = {value}")
        .parse::<Table>()
        .ok()
        .and_then(|mut table| table.remove("value"))
        .unwrap_or_else(|| TomlValue::String(value.to_owned()))
}

fn set_dotted_value(table: &mut Table, key: &str, value: TomlValue) -> Result<(), CliFailure> {
    let parts = dotted_key_parts(key)?;
    let mut current = table;
    for part in &parts[..parts.len() - 1] {
        let entry = current
            .entry((*part).to_owned())
            .or_insert_with(|| TomlValue::Table(Table::new()));
        current = entry
            .as_table_mut()
            .ok_or_else(|| CliFailure::new(1, format!("config key {part:?} is not a table")))?;
    }
    current.insert(parts[parts.len() - 1].to_owned(), value);
    Ok(())
}

fn unset_dotted_value(table: &mut Table, key: &str) -> Result<(), CliFailure> {
    let parts = dotted_key_parts(key)?;
    let mut current = table;
    for part in &parts[..parts.len() - 1] {
        let Some(next) = current.get_mut(*part).and_then(TomlValue::as_table_mut) else {
            return Ok(());
        };
        current = next;
    }
    current.remove(parts[parts.len() - 1]);
    Ok(())
}

fn dotted_key_parts(key: &str) -> Result<Vec<&str>, CliFailure> {
    let parts = key.split('.').collect::<Vec<_>>();
    if parts.is_empty() || parts.iter().any(|part| part.trim().is_empty()) {
        return Err(CliFailure::new(
            1,
            "config key must be a non-empty dotted key",
        ));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::config::LocalOverlaySource;
    use crate::machine_identity::machine_id_path;

    fn loaded_config(root: &Path) -> LoadedConfig {
        LoadedConfig {
            data: Table::new(),
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn set_writes_local_config_and_parses_toml_values() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(temp.path());
        let mut stdout = Vec::new();

        config_set(
            ConfigWriteContext {
                mode: RuntimeMode::Shipyard,
                cwd: temp.path(),
                config: &config,
            },
            "multi_host.controller.enabled",
            "true",
            ConfigScope::Local,
            true,
            &mut stdout,
        )
        .expect("set");

        let text =
            fs::read_to_string(temp.path().join(".shipyard.local/config.toml")).expect("config");
        let table = text.parse::<Table>().expect("toml");
        assert_eq!(
            table
                .get("multi_host")
                .and_then(TomlValue::as_table)
                .and_then(|multi| multi.get("controller"))
                .and_then(TomlValue::as_table)
                .and_then(|controller| controller.get("enabled"))
                .and_then(TomlValue::as_bool),
            Some(true)
        );
        let body: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(body["command"], "config.set");
        assert_eq!(body["scope"], "local");
    }

    #[test]
    fn set_bare_words_as_strings_and_unset_removes_key() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(temp.path());
        let mut stdout = Vec::new();

        config_set(
            ConfigWriteContext {
                mode: RuntimeMode::Shipyard,
                cwd: temp.path(),
                config: &config,
            },
            "multi_host.controller.name",
            "mac-studio",
            ConfigScope::Project,
            false,
            &mut stdout,
        )
        .expect("set");
        config_unset(
            RuntimeMode::Shipyard,
            temp.path(),
            &config,
            "multi_host.controller.name",
            ConfigScope::Project,
            false,
            &mut stdout,
        )
        .expect("unset");

        let text = fs::read_to_string(temp.path().join(".shipyard/config.toml")).expect("config");
        assert!(!text.contains("name ="));
    }

    #[test]
    fn set_refuses_to_overwrite_scalar_parent() {
        let mut table = r#"multi_host = "not-a-table""#.parse::<Table>().expect("toml");

        let error = set_dotted_value(
            &mut table,
            "multi_host.controller.enabled",
            TomlValue::Boolean(true),
        )
        .expect_err("scalar parent");

        assert!(error.message.contains("not a table"));
    }

    #[test]
    fn export_setup_bundle_includes_layers_and_strips_secrets() {
        let temp = TempDir::new().expect("tempdir");
        let global_dir = temp.path().join("global");
        let project_dir = temp.path().join(".shipyard");
        let local_dir = temp.path().join(".shipyard.local");
        fs::create_dir_all(&global_dir).expect("global");
        fs::create_dir_all(&project_dir).expect("project");
        fs::create_dir_all(&local_dir).expect("local");
        fs::write(
            global_dir.join("config.toml"),
            r#"
            [github.auth]
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            token = "ghp_secret"

            [multi_host.controller]
            enabled = true
            private_key = "secret"
            app_pem = "secret"
            db_credentials = "secret"
            "#,
        )
        .expect("global config");
        fs::write(
            local_dir.join("config.toml"),
            r#"
            [multi_host.controller]
            name = "mac-studio"
            "#,
        )
        .expect("local config");
        let config = LoadedConfig {
            data: r#"
                [github.auth]
                source = "env"
                token_env = "SHIPYARD_GITHUB_TOKEN"
                token = "ghp_secret"
            "#
            .parse::<Table>()
            .expect("effective"),
            global_dir,
            project_dir: Some(project_dir),
            local_dir: Some(local_dir),
            local_overlay_source: LocalOverlaySource::Direct,
        };

        let bundle = export_setup_bundle(
            RuntimeMode::Shipyard,
            temp.path().join("state").as_path(),
            &config,
            false,
        )
        .expect("bundle");
        let text = bundle.to_string();

        assert!(text.contains("[shipyard_setup]"));
        assert!(text.contains("token_env = \"SHIPYARD_GITHUB_TOKEN\""));
        assert!(text.contains("name = \"mac-studio\""));
        assert!(!text.contains("ghp_secret"));
        assert!(!text.contains("private_key"));
        assert!(!text.contains("app_pem"));
        assert!(!text.contains("db_credentials"));
        assert!(!machine_id_path(&temp.path().join("state")).exists());
    }

    #[test]
    fn export_setup_bundle_reports_existing_machine_id_without_creating_one() {
        let temp = TempDir::new().expect("tempdir");
        let state_dir = temp.path().join("state");
        let path = machine_id_path(&state_dir);
        fs::create_dir_all(path.parent().expect("parent")).expect("state");
        fs::write(&path, "sy_node_0123456789abcdef0123456789abcdef\n").expect("machine-id");
        let config = LoadedConfig {
            data: Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        };

        let bundle =
            export_setup_bundle(RuntimeMode::Shipyard, &state_dir, &config, false).expect("bundle");

        assert!(
            bundle
                .to_string()
                .contains("machine_id = \"sy_node_0123456789abcdef0123456789abcdef\"")
        );
    }

    #[test]
    fn export_setup_bundle_can_include_secrets_for_private_backup() {
        let temp = TempDir::new().expect("tempdir");
        let local_dir = temp.path().join(".shipyard.local");
        fs::create_dir_all(&local_dir).expect("local");
        fs::write(
            local_dir.join("config.toml"),
            r#"
            [multi_host.client]
            enabled = true
            controller = "ssh://mac-studio"
            node_token = "synode_secret"
            "#,
        )
        .expect("local config");
        let config = LoadedConfig {
            data: r#"
                [multi_host.client]
                enabled = true
                controller = "ssh://mac-studio"
                node_token = "synode_secret"
            "#
            .parse::<Table>()
            .expect("effective"),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: Some(local_dir),
            local_overlay_source: LocalOverlaySource::Direct,
        };

        let safe = export_setup_bundle(RuntimeMode::Shipyard, temp.path(), &config, false)
            .expect("safe bundle")
            .to_string();
        let private = export_setup_bundle(RuntimeMode::Shipyard, temp.path(), &config, true)
            .expect("private bundle")
            .to_string();

        assert!(!safe.contains("node_token"));
        assert!(!safe.contains("synode_secret"));
        assert!(private.contains("node_token = \"synode_secret\""));
        assert!(private.contains("includes raw config secrets"));
        assert!(private.contains("Treat this file like a private key"));
    }

    #[test]
    fn include_secrets_export_requires_output() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(temp.path());
        let mut stdout = Vec::new();

        let error = config_export(
            RuntimeMode::Shipyard,
            temp.path(),
            &config,
            None,
            true,
            false,
            &mut stdout,
        )
        .expect_err("stdout secret export should fail");

        assert_eq!(error.code, 2);
        assert!(
            error
                .message
                .contains("requires --output to avoid writing secrets to stdout")
        );
        assert!(stdout.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn include_secrets_export_writes_owner_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("tempdir");
        let local_dir = temp.path().join(".shipyard.local");
        fs::create_dir_all(&local_dir).expect("local");
        fs::write(
            local_dir.join("config.toml"),
            r#"
            [multi_host.client]
            node_token = "synode_secret"
            "#,
        )
        .expect("local config");
        let config = LoadedConfig {
            data: Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: Some(local_dir),
            local_overlay_source: LocalOverlaySource::Direct,
        };
        let output = temp.path().join("shipyard-private-setup.toml");
        let mut stdout = Vec::new();

        config_export(
            RuntimeMode::Shipyard,
            temp.path(),
            &config,
            Some(&output),
            true,
            false,
            &mut stdout,
        )
        .expect("export");

        let mode = fs::metadata(&output)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn include_secrets_export_json_omits_bundle_from_stdout() {
        let temp = TempDir::new().expect("tempdir");
        let local_dir = temp.path().join(".shipyard.local");
        fs::create_dir_all(&local_dir).expect("local");
        fs::write(
            local_dir.join("config.toml"),
            r#"
            [multi_host.client]
            node_token = "synode_secret"
            "#,
        )
        .expect("local config");
        let config = LoadedConfig {
            data: Table::new(),
            global_dir: temp.path().join("global"),
            project_dir: None,
            local_dir: Some(local_dir),
            local_overlay_source: LocalOverlaySource::Direct,
        };
        let output = temp.path().join("shipyard-private-setup.toml");
        let mut stdout = Vec::new();

        config_export(
            RuntimeMode::Shipyard,
            temp.path(),
            &config,
            Some(&output),
            true,
            true,
            &mut stdout,
        )
        .expect("export");

        let rendered = String::from_utf8(stdout).expect("utf8");
        assert!(!rendered.contains("synode_secret"));
        assert!(!rendered.contains("node_token"));
        let payload: Value = serde_json::from_str(&rendered).expect("json");
        assert_eq!(payload["command"], "config.export");
        assert_eq!(payload["include_secrets"], true);
        assert!(payload.get("bundle").is_none());
        assert_eq!(payload["path"], output.display().to_string());
    }

    #[test]
    fn import_setup_bundle_writes_selected_layer_to_scope() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(temp.path());
        let input = temp.path().join("setup.toml");
        fs::write(
            &input,
            r#"
            [shipyard_setup]
            format = "shipyard-setup"
            version = 1

            [config.local.multi_host.controller]
            name = "mac-studio"
            enabled = true
            "#,
        )
        .expect("bundle");
        let mut stdout = Vec::new();

        config_import(
            ConfigWriteContext {
                mode: RuntimeMode::Shipyard,
                cwd: temp.path(),
                config: &config,
            },
            &input,
            ConfigBundleLayer::Local,
            ConfigScope::Local,
            true,
            &mut stdout,
        )
        .expect("import");

        let text =
            fs::read_to_string(temp.path().join(".shipyard.local/config.toml")).expect("config");
        assert!(text.contains("name = \"mac-studio\""));
        assert!(text.contains("enabled = true"));
        let body: Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(body["command"], "config.import");
        assert_eq!(body["from"], "local");
    }
}
