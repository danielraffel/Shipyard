use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Value as JsonValue, json};
use toml::{Table, Value as TomlValue};

use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::doctor::{DoctorEntry, check_github_auth};
use crate::gh::GhClient;
use crate::identity::{ProductIdentity, RuntimeMode};
use crate::output::write_json_envelope;

use super::{
    CliFailure,
    cli::{AuthCommand, AuthConfigScope},
};

pub(super) fn auth_command<W: Write>(
    command: AuthCommand,
    mode: RuntimeMode,
    cwd: &Path,
    global_dir: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        AuthCommand::Doctor => {
            auth_doctor(mode, cwd, json_mode, stdout)?;
        }
        AuthCommand::Export { output } => {
            let config = LoadedConfig::load_from_cwd(mode, cwd)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            auth_export(&config, output.as_deref(), json_mode, stdout)?;
        }
        AuthCommand::Import { input, scope } => {
            let config = LoadedConfig::load_from_cwd(mode, cwd)
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
            auth_import(
                mode, cwd, global_dir, &config, &input, scope, json_mode, stdout,
            )?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn auth_doctor<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let entry = check_github_auth(mode, cwd);
    if json_mode {
        let mut data = BTreeMap::new();
        data.insert(
            "auth".to_owned(),
            serde_json::to_value(&entry).map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        return write_json_envelope(stdout, "auth.doctor", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(stdout, "shipyard auth doctor").map_err(io_err)?;
    write_entry(stdout, "github-auth", &entry).map_err(io_err)
}

fn auth_export<W: Write>(
    config: &LoadedConfig,
    output: Option<&Path>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let bundle = export_bundle(config);
    let text = format!("{bundle}\n");
    if let Some(path) = output {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
        fs::write(path, text).map_err(|error| CliFailure::new(1, error.to_string()))?;
        if json_mode {
            let mut data = BTreeMap::new();
            data.insert("path".to_owned(), json!(path.to_string_lossy()));
            data.insert("bundle".to_owned(), json_table(&bundle)?);
            return write_json_envelope(stdout, "auth.export", data)
                .map_err(|error| CliFailure::new(1, error.to_string()));
        }
        writeln!(stdout, "Wrote {}", path.display()).map_err(io_err)?;
        return Ok(());
    }

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("bundle".to_owned(), json_table(&bundle)?);
        write_json_envelope(stdout, "auth.export", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        write!(stdout, "{text}").map_err(io_err)
    }
}

fn auth_import<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    global_dir: &Path,
    config: &LoadedConfig,
    input: &Path,
    scope: AuthConfigScope,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let text = fs::read_to_string(input).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let bundle = text
        .parse::<Table>()
        .map_err(|error| CliFailure::new(1, format!("failed to parse auth bundle: {error}")))?;
    let auth = auth_table_from_bundle(&bundle)?;
    reject_unknown_auth_keys(&auth)?;
    validate_auth_table(&auth, config)?;
    let path = config_path_for_scope(mode, cwd, global_dir, config, scope);
    write_auth_table(&path, auth)?;

    if json_mode {
        let mut data = BTreeMap::new();
        data.insert("path".to_owned(), json!(path.to_string_lossy()));
        data.insert("scope".to_owned(), json!(scope_label(scope)));
        return write_json_envelope(stdout, "auth.import", data)
            .map_err(|error| CliFailure::new(1, error.to_string()));
    }
    writeln!(
        stdout,
        "Imported GitHub auth config into {}",
        path.display()
    )
    .map_err(io_err)
}

fn export_bundle(config: &LoadedConfig) -> Table {
    let auth = config
        .get("github.auth")
        .and_then(TomlValue::as_table)
        .cloned()
        .unwrap_or_else(ambient_auth_table);

    let mut github = Table::new();
    github.insert("auth".to_owned(), TomlValue::Table(auth.clone()));

    let mut bundle = Table::new();
    bundle.insert("version".to_owned(), TomlValue::Integer(1));
    bundle.insert("github".to_owned(), TomlValue::Table(github));
    bundle.insert(
        "requirements".to_owned(),
        TomlValue::Table(requirements_table(&auth)),
    );
    bundle
}

fn ambient_auth_table() -> Table {
    let mut table = Table::new();
    table.insert("source".to_owned(), TomlValue::String("gh-cli".to_owned()));
    table
}

fn requirements_table(auth: &Table) -> Table {
    let mut table = Table::new();
    let mut commands = Vec::new();
    let mut env_vars = Vec::new();
    let mut notes = Vec::new();
    match auth.get("source").and_then(TomlValue::as_str) {
        Some("env") => {
            if let Some(token_env) = auth.get("token_env").and_then(TomlValue::as_str) {
                env_vars.push(TomlValue::String(token_env.to_owned()));
                notes.push(TomlValue::String(format!(
                    "Set {token_env} on the destination machine before using Shipyard."
                )));
            }
        }
        Some("command") => {
            if let Some(command) = auth.get("token_command").and_then(TomlValue::as_array)
                && let Some(program) = command.first().and_then(TomlValue::as_str)
            {
                commands.push(TomlValue::String(program.to_owned()));
                notes.push(TomlValue::String(format!(
                    "Install and authenticate {program} on the destination machine."
                )));
            }
            notes.push(TomlValue::String(
                "Token helpers must not write secrets to stderr.".to_owned(),
            ));
        }
        _ => {
            commands.push(TomlValue::String("gh".to_owned()));
            notes.push(TomlValue::String(
                "Run gh auth login or gh auth refresh on the destination machine.".to_owned(),
            ));
        }
    }
    table.insert("commands".to_owned(), TomlValue::Array(commands));
    table.insert("env_vars".to_owned(), TomlValue::Array(env_vars));
    table.insert("notes".to_owned(), TomlValue::Array(notes));
    table
}

fn auth_table_from_bundle(bundle: &Table) -> Result<Table, CliFailure> {
    if bundle.get("version").and_then(TomlValue::as_integer) != Some(1) {
        return Err(CliFailure::new(
            1,
            "unsupported auth bundle version; expected version = 1",
        ));
    }
    bundle
        .get("github")
        .and_then(TomlValue::as_table)
        .and_then(|github| github.get("auth"))
        .and_then(TomlValue::as_table)
        .cloned()
        .ok_or_else(|| CliFailure::new(1, "auth bundle missing [github.auth]"))
}

fn validate_auth_table(auth: &Table, reference: &LoadedConfig) -> Result<(), CliFailure> {
    let mut github = Table::new();
    github.insert("auth".to_owned(), TomlValue::Table(auth.clone()));
    let mut data = Table::new();
    data.insert("github".to_owned(), TomlValue::Table(github));
    let config = LoadedConfig {
        data,
        global_dir: reference.global_dir.clone(),
        project_dir: reference.project_dir.clone(),
        local_dir: reference.local_dir.clone(),
        local_overlay_source: reference.local_overlay_source,
    };
    GhClient::from_loaded_config(&config)
        .map(|_| ())
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn reject_unknown_auth_keys(auth: &Table) -> Result<(), CliFailure> {
    const ALLOWED: &[&str] = &[
        "source",
        "token_env",
        "token_command",
        "cache_ttl_seconds",
        "refresh_skew_seconds",
    ];
    if let Some(key) = auth.keys().find(|key| !ALLOWED.contains(&key.as_str())) {
        return Err(CliFailure::new(
            1,
            format!("unsupported github.auth key {key:?} in auth bundle"),
        ));
    }
    Ok(())
}

fn config_path_for_scope(
    mode: RuntimeMode,
    cwd: &Path,
    global_dir: &Path,
    config: &LoadedConfig,
    scope: AuthConfigScope,
) -> PathBuf {
    let identity = ProductIdentity::for_mode(mode);
    match scope {
        AuthConfigScope::Global => global_dir.join("config.toml"),
        AuthConfigScope::Project => config
            .project_dir
            .clone()
            .unwrap_or_else(|| cwd.join(identity.tracked_project_dir_name))
            .join("config.toml"),
        AuthConfigScope::Local => {
            let local_dir = match config.local_overlay_source {
                LocalOverlaySource::Direct => config
                    .local_dir
                    .clone()
                    .unwrap_or_else(|| cwd.join(identity.local_overlay_dir_name)),
                LocalOverlaySource::WorktreeFallback | LocalOverlaySource::None => {
                    cwd.join(identity.local_overlay_dir_name)
                }
            };
            local_dir.join("config.toml")
        }
    }
}

fn write_auth_table(path: &Path, auth: Table) -> Result<(), CliFailure> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    let mut config = if path.exists() {
        fs::read_to_string(path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?
            .parse::<Table>()
            .map_err(|error| CliFailure::new(1, format!("failed to parse config: {error}")))?
    } else {
        Table::new()
    };
    let github = config
        .entry("github".to_owned())
        .or_insert_with(|| TomlValue::Table(Table::new()))
        .as_table_mut()
        .ok_or_else(|| CliFailure::new(1, "[github] config is not a table"))?;
    github.insert("auth".to_owned(), TomlValue::Table(auth));
    fs::write(path, format!("{config}\n")).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn json_table(table: &Table) -> Result<JsonValue, CliFailure> {
    serde_json::to_value(table).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn scope_label(scope: AuthConfigScope) -> &'static str {
    match scope {
        AuthConfigScope::Global => "global",
        AuthConfigScope::Project => "project",
        AuthConfigScope::Local => "local",
    }
}

fn write_entry<W: Write>(stdout: &mut W, name: &str, entry: &DoctorEntry) -> std::io::Result<()> {
    let status = if entry.ok { "ok" } else { "fail" };
    let summary = entry
        .version
        .as_deref()
        .or(entry.error.as_deref())
        .unwrap_or("");
    writeln!(stdout, "  {name}: {status} {summary}")?;
    if let Some(detail) = entry.detail.as_deref()
        && detail != summary
    {
        for line in detail.lines().filter(|line| !line.trim().is_empty()) {
            writeln!(stdout, "    {line}")?;
        }
    }
    Ok(())
}

fn io_err(error: std::io::Error) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;

    fn loaded_config(root: &Path, contents: &str) -> LoadedConfig {
        LoadedConfig {
            data: contents.parse::<Table>().expect("config TOML"),
            global_dir: root.join("global"),
            project_dir: Some(root.join(".shipyard")),
            local_dir: Some(root.join(".shipyard.local")),
            local_overlay_source: LocalOverlaySource::Direct,
        }
    }

    #[test]
    fn export_bundle_contains_env_requirements_without_secret() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(
            temp.path(),
            r#"
            [github.auth]
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            "#,
        );

        let bundle = export_bundle(&config);

        assert_eq!(
            bundle
                .get("github")
                .and_then(TomlValue::as_table)
                .and_then(|github| github.get("auth"))
                .and_then(TomlValue::as_table)
                .and_then(|auth| auth.get("token_env"))
                .and_then(TomlValue::as_str),
            Some("SHIPYARD_GITHUB_TOKEN")
        );
        assert!(!bundle.to_string().contains("ghp_"));
        assert!(
            bundle
                .get("requirements")
                .and_then(TomlValue::as_table)
                .and_then(|req| req.get("env_vars"))
                .and_then(TomlValue::as_array)
                .is_some_and(|vars| vars
                    .iter()
                    .any(|value| value.as_str() == Some("SHIPYARD_GITHUB_TOKEN")))
        );
    }

    #[test]
    fn import_writes_local_overlay_auth_config() {
        let temp = TempDir::new().expect("tempdir");
        let config = loaded_config(temp.path(), "");
        let input = temp.path().join("auth.toml");
        fs::write(
            &input,
            r#"
            version = 1

            [github.auth]
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            "#,
        )
        .expect("bundle");
        let mut output = Vec::new();

        auth_import(
            RuntimeMode::Shipyard,
            temp.path(),
            &temp.path().join("global"),
            &config,
            &input,
            AuthConfigScope::Local,
            false,
            &mut output,
        )
        .expect("import");

        let text =
            fs::read_to_string(temp.path().join(".shipyard.local/config.toml")).expect("config");
        assert!(text.contains("[github.auth]"));
        assert!(text.contains("token_env = \"SHIPYARD_GITHUB_TOKEN\""));
    }

    #[test]
    fn invalid_bundle_version_is_rejected() {
        let bundle = "version = 2\n[github.auth]\nsource = \"gh-cli\"\n"
            .parse::<Table>()
            .expect("bundle");

        let error = auth_table_from_bundle(&bundle).expect_err("invalid version");

        assert!(error.message.contains("unsupported auth bundle version"));
    }

    #[test]
    fn import_rejects_unknown_auth_keys() {
        let auth = r#"
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            token = "ghp_secret"
        "#
        .parse::<Table>()
        .expect("auth");

        let error = reject_unknown_auth_keys(&auth).expect_err("unknown key");

        assert!(error.message.contains("unsupported github.auth key"));
    }

    #[test]
    fn scope_paths_are_mode_aware() {
        let temp = TempDir::new().expect("tempdir");
        let config = LoadedConfig {
            data: Table::new(),
            global_dir: temp.path().join("ignored-global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        };

        assert_eq!(
            config_path_for_scope(
                RuntimeMode::Shipyard,
                temp.path(),
                &PathBuf::from("/tmp/global"),
                &config,
                AuthConfigScope::Local,
            ),
            temp.path().join(".shipyard.local/config.toml")
        );
    }
}
