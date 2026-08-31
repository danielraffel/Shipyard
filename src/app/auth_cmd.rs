use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use atomicwrites::{AllowOverwrite, AtomicFile};
use serde::Serialize;
use serde_json::{Value as JsonValue, json};
use sha2::{Digest, Sha256};
use toml::{Table, Value as TomlValue};
use toml_edit::DocumentMut;

use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::doctor::DoctorEntry;
use crate::gh::GhClient;
use crate::identity::{ProductIdentity, RuntimeMode};
use crate::output::{SCHEMA_VERSION, write_json_envelope, write_pretty_json};

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
        AuthCommand::HelperArgv { wrapper, repo } => {
            auth_helper_argv(global_dir, &wrapper, &repo, stdout)?;
        }
        AuthCommand::Doctor { repo } => {
            auth_doctor(mode, cwd, repo.as_deref(), json_mode, stdout)?;
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

#[derive(Debug, Eq, PartialEq, Serialize)]
struct HelperArgvOutput {
    schema_version: u32,
    command: &'static str,
    wrapper: String,
    repo: String,
    credential_argv: Vec<String>,
}

const REQUIRED_GENERATION_MANIFEST_KEYS: &[&str] = &[
    "schema_version",
    "generation_contract",
    "generation_id",
    "authority_identity",
    "helper_sha256",
    "helper_mode",
    "wrapper_sha256",
    "wrapper_mode",
    "public_trampoline_sha256",
    "public_trampoline_mode",
    "close_guard_sha256",
    "close_guard_mode",
    "binary_sha256",
    "binary_mode",
    "companion_sha256",
    "context_sha256",
    "context_template_sha256",
];

fn auth_helper_argv<W: Write>(
    global_dir: &Path,
    wrapper: &Path,
    repo: &str,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    let mut config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if remediate_installed_generation_wrapper(global_dir, wrapper, repo, &config)? {
        config = LoadedConfig::load_machine_global_from_dir(global_dir.to_path_buf())
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    let output = resolve_helper_argv(&config, wrapper, repo)?;
    write_pretty_json(stdout, &output).map_err(|error| CliFailure::new(1, error.to_string()))
}

fn remediate_installed_generation_wrapper(
    global_dir: &Path,
    wrapper: &Path,
    repo: &str,
    config: &LoadedConfig,
) -> Result<bool, CliFailure> {
    let requested_wrapper = exact_absolute_path(wrapper, "--wrapper")?;
    let Some(auth) = config.get("github.auth").and_then(TomlValue::as_table) else {
        return Ok(false);
    };
    let Some(command) = auth.get("token_command").and_then(TomlValue::as_array) else {
        return Ok(false);
    };
    let Some(configured_wrapper) = command.first().and_then(TomlValue::as_str) else {
        return Ok(false);
    };
    if configured_wrapper == requested_wrapper {
        return Ok(false);
    }
    let configured_path = Path::new(configured_wrapper);
    resolve_helper_argv(config, configured_path, repo)?;
    if !is_safe_installed_generation_alias(wrapper, configured_path)? {
        return Ok(false);
    }

    let config_path = global_dir.join("config.toml");
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(&config_path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let text =
        fs::read_to_string(&config_path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut document = text
        .parse::<DocumentMut>()
        .map_err(|error| CliFailure::new(1, format!("failed to parse config: {error}")))?;
    let current_command = document
        .get_mut("github")
        .and_then(github_auth_token_command_mut)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "machine-global github.auth.token_command changed during wrapper remediation",
            )
        })?;
    if current_command.get(0).and_then(toml_edit::Value::as_str) != Some(configured_wrapper) {
        return Err(CliFailure::new(
            1,
            "machine-global github.auth.token_command changed during wrapper remediation",
        ));
    }
    let selector_path = wrapper.with_extension("shipyard-generation");
    let selector_target = fs::read_link(&selector_path).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "failed to read installed auth generation selector {}: {error}",
                selector_path.display()
            ),
        )
    })?;
    if selector_target != Path::new(configured_wrapper) {
        return Err(CliFailure::new(
            1,
            "machine-global auth wrapper disagreed with the installed generation selector",
        ));
    }
    current_command.replace(0, toml_edit::Value::from(requested_wrapper));
    write_config_atomically(&config_path, &document.to_string())?;
    Ok(true)
}

fn github_auth_token_command_mut(item: &mut toml_edit::Item) -> Option<&mut toml_edit::Array> {
    match item {
        toml_edit::Item::Table(github) => match github.get_mut("auth")? {
            toml_edit::Item::Table(auth) => auth.get_mut("token_command")?.as_array_mut(),
            toml_edit::Item::Value(toml_edit::Value::InlineTable(auth)) => {
                auth.get_mut("token_command")?.as_array_mut()
            }
            _ => None,
        },
        toml_edit::Item::Value(toml_edit::Value::InlineTable(github)) => github
            .get_mut("auth")?
            .as_inline_table_mut()?
            .get_mut("token_command")?
            .as_array_mut(),
        _ => None,
    }
}

fn is_safe_installed_generation_alias(
    public_wrapper: &Path,
    configured_wrapper: &Path,
) -> Result<bool, CliFailure> {
    let configured_wrapper = configured_wrapper
        .canonicalize()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let Some(bin_dir) = public_wrapper.parent() else {
        return Ok(false);
    };
    let Some(local_dir) = bin_dir.parent() else {
        return Ok(false);
    };
    let Some(home_dir) = local_dir.parent() else {
        return Ok(false);
    };
    let public_metadata = public_wrapper
        .symlink_metadata()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let public_source = fs::read_to_string(public_wrapper)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if public_wrapper.file_name().and_then(|name| name.to_str()) != Some("ghapp")
        || bin_dir.file_name().and_then(|name| name.to_str()) != Some("bin")
        || local_dir.file_name().and_then(|name| name.to_str()) != Some(".local")
        || !public_metadata.is_file()
        || !has_private_executable_mode(&public_metadata)
        || public_source
            .matches("# Shipyard-Stable-Public-Trampoline-Contract: stable-selector-v1")
            .count()
            != 1
    {
        return Ok(false);
    }
    let generation_root = home_dir
        .join(".local/share/shipyard/auth-generations")
        .canonicalize()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let Ok(relative) = configured_wrapper.strip_prefix(&generation_root) else {
        return Ok(false);
    };
    let components = relative.components().collect::<Vec<_>>();
    let Some(generation_id) = components
        .first()
        .and_then(|part| part.as_os_str().to_str())
    else {
        return Ok(false);
    };
    if components.len() != 2
        || generation_id.len() != 64
        || !generation_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || components[1].as_os_str() != "ghapp"
    {
        return Ok(false);
    }
    let metadata = configured_wrapper
        .symlink_metadata()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !metadata.is_file() || !has_private_executable_mode(&metadata) {
        return Ok(false);
    }
    is_valid_generation_manifest(
        &configured_wrapper.with_file_name("generation.manifest"),
        &configured_wrapper,
        public_wrapper,
        generation_id,
    )
}

fn is_valid_generation_manifest(
    manifest_path: &Path,
    configured_wrapper: &Path,
    public_wrapper: &Path,
    generation_id: &str,
) -> Result<bool, CliFailure> {
    let manifest_metadata = manifest_path
        .symlink_metadata()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !manifest_metadata.is_file() || !has_private_mode(&manifest_metadata, 0o600) {
        return Ok(false);
    }
    let manifest =
        fs::read_to_string(manifest_path).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let mut values = BTreeMap::new();
    for line in manifest.lines() {
        let Some((key, value)) = line.split_once('=') else {
            return Ok(false);
        };
        if key.is_empty() || value.is_empty() || values.insert(key, value).is_some() {
            return Ok(false);
        }
    }
    if values.len() != REQUIRED_GENERATION_MANIFEST_KEYS.len()
        || !REQUIRED_GENERATION_MANIFEST_KEYS
            .iter()
            .all(|key| values.contains_key(key))
    {
        return Ok(false);
    }
    let Some(generation_dir) = configured_wrapper.parent() else {
        return Ok(false);
    };
    let member_matches = |name: &str, digest_key: &str, mode: u32| {
        generation_member_matches(
            &generation_dir.join(name),
            values.get(digest_key).copied(),
            mode,
        )
    };
    let companion_matches = match values.get("companion_sha256").copied() {
        Some("absent") => {
            path_absent_no_follow(&generation_dir.join("shipyard-workstream-provider"))
        }
        digest => generation_member_matches(
            &generation_dir.join("shipyard-workstream-provider"),
            digest,
            0o700,
        )?,
    };
    let context_matches = match values.get("context_sha256").copied() {
        Some("absent") => {
            path_absent_no_follow(&generation_dir.join("ghapp.shipyard-context.json"))
        }
        digest => generation_member_matches(
            &generation_dir.join("ghapp.shipyard-context.json"),
            digest,
            0o600,
        )?,
    };
    let public_wrapper_matches = generation_member_matches(
        public_wrapper,
        values.get("public_trampoline_sha256").copied(),
        0o700,
    )?;
    Ok(values.get("schema_version") == Some(&"1")
        && values.get("generation_contract") == Some(&"auth-selector-v2")
        && values.get("generation_id") == Some(&generation_id)
        && values.get("wrapper_mode") == Some(&"700")
        && values.get("helper_mode") == Some(&"700")
        && values.get("public_trampoline_mode") == Some(&"700")
        && values.get("close_guard_mode") == Some(&"700")
        && values.get("binary_mode") == Some(&"700")
        && values
            .get("authority_identity")
            .is_some_and(|value| valid_lower_sha256(value))
        && values
            .get("context_template_sha256")
            .is_some_and(|value| valid_lower_sha256(value))
        && member_matches("shipyard-github-app-token", "helper_sha256", 0o700)?
        && member_matches("ghapp", "wrapper_sha256", 0o700)?
        && member_matches("ghapp.public-trampoline", "public_trampoline_sha256", 0o700)?
        && member_matches("pr-close-guard", "close_guard_sha256", 0o700)?
        && member_matches("shipyard", "binary_sha256", 0o700)?
        && companion_matches
        && context_matches
        && public_wrapper_matches)
}

fn has_private_executable_mode(metadata: &fs::Metadata) -> bool {
    has_private_mode(metadata, 0o700)
}

fn has_private_mode(metadata: &fs::Metadata, expected: u32) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777 == expected
    }
    #[cfg(not(unix))]
    {
        let _ = (metadata, expected);
        true
    }
}

fn generation_member_matches(
    path: &Path,
    expected_digest: Option<&str>,
    expected_mode: u32,
) -> Result<bool, CliFailure> {
    let Some(expected_digest) = expected_digest else {
        return Ok(false);
    };
    let metadata = path
        .symlink_metadata()
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if !metadata.is_file()
        || !has_private_mode(&metadata, expected_mode)
        || !valid_lower_sha256(expected_digest)
    {
        return Ok(false);
    }
    let observed = format!(
        "{:x}",
        Sha256::digest(fs::read(path).map_err(|error| CliFailure::new(1, error.to_string()))?)
    );
    Ok(observed == expected_digest)
}

fn valid_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn path_absent_no_follow(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn write_config_atomically(path: &Path, text: &str) -> Result<(), CliFailure> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    AtomicFile::new(path, AllowOverwrite)
        .write_with_options(|file| file.write_all(text.as_bytes()), options)
        .map_err(|error| CliFailure::new(1, error.to_string()))
}

fn resolve_helper_argv(
    config: &LoadedConfig,
    wrapper: &Path,
    repo: &str,
) -> Result<HelperArgvOutput, CliFailure> {
    let wrapper = exact_absolute_path(wrapper, "--wrapper")?;
    if !is_exact_repo_slug(repo) {
        return Err(CliFailure::new(
            2,
            "auth helper-argv --repo must be an exact OWNER/REPO slug",
        ));
    }
    let auth = config
        .get("github.auth")
        .and_then(TomlValue::as_table)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "machine-global config is missing [github.auth] for ghapp resolver",
            )
        })?;
    if auth.get("source").and_then(TomlValue::as_str) != Some("command") {
        return Err(CliFailure::new(
            1,
            "machine-global github.auth.source must be \"command\" for ghapp resolver",
        ));
    }
    let command = auth
        .get("token_command")
        .and_then(TomlValue::as_array)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "machine-global github.auth.token_command is missing or is not an array",
            )
        })?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            CliFailure::new(
                1,
                "machine-global github.auth.token_command must contain only strings",
            )
        })?;
    if command.len() != 8
        || command[0] != wrapper
        || command[1] != "token"
        || command[2] != "--app-id"
        || command[4] != "--private-key"
        || command[6] != "--repo"
        || command[7] != "{repo_slug}"
    {
        return Err(CliFailure::new(
            1,
            "machine-global github.auth.token_command must exactly match [WRAPPER, \"token\", \"--app-id\", VALUE, \"--private-key\", ABSOLUTE_PATH, \"--repo\", \"{repo_slug}\"]",
        ));
    }
    if !is_nonzero_ascii_decimal(&command[3]) {
        return Err(CliFailure::new(
            1,
            "machine-global github.auth.token_command --app-id must be a nonzero ASCII decimal",
        ));
    }
    exact_absolute_path(Path::new(&command[5]), "token_command --private-key")?;

    Ok(HelperArgvOutput {
        schema_version: SCHEMA_VERSION,
        command: "auth.helper-argv",
        wrapper,
        repo: repo.to_owned(),
        credential_argv: vec![
            "--app-id".to_owned(),
            command[3].clone(),
            "--private-key".to_owned(),
            command[5].clone(),
        ],
    })
}

fn is_nonzero_ascii_decimal(value: &str) -> bool {
    (1..=20).contains(&value.len())
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|number| number != 0)
}

fn exact_absolute_path(path: &Path, label: &str) -> Result<String, CliFailure> {
    let raw = path.to_str().ok_or_else(|| {
        CliFailure::new(2, format!("auth helper-argv {label} must be valid UTF-8"))
    })?;
    if !path.is_absolute()
        || !(2..=4096).contains(&raw.len())
        || raw.chars().any(char::is_control)
        || raw
            .split('/')
            .skip(1)
            .any(|component| matches!(component, "" | "." | ".."))
    {
        return Err(CliFailure::new(
            2,
            format!("auth helper-argv {label} must be a normalized absolute path"),
        ));
    }
    Ok(raw.to_owned())
}

fn is_exact_repo_slug(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    !name.contains('/')
        && (1..=39).contains(&owner.len())
        && owner
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && owner
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !matches!(name, "" | "." | "..")
        && name.len() <= 255
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn auth_doctor<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    repo: Option<&str>,
    json_mode: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if let Some(repo) = repo {
        crate::gh::validate_repo_slug(repo)
            .map_err(|error| CliFailure::new(2, error.to_string()))?;
    }
    let entry = crate::doctor::check_github_auth_for_repo(mode, cwd, repo);
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
        let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
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

#[allow(clippy::too_many_arguments)]
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
    if let Some(binary) = auth.get("ambient_gh_binary").and_then(TomlValue::as_str) {
        commands.push(TomlValue::String(binary.to_owned()));
        notes.push(TomlValue::String(
            "The ambient gh binary must be a direct native GitHub CLI executable authenticated with the destination machine keyring; scripts and wrappers are rejected."
                .to_owned(),
        ));
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
        "ambient_gh_binary",
        "privileged_gh_binary",
        "privileged_git_binary",
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
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
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

#[allow(clippy::needless_pass_by_value)]
fn io_err(error: std::io::Error) -> CliFailure {
    CliFailure::new(1, error.to_string())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    use super::*;

    const TEST_WRAPPER: &str = if cfg!(windows) {
        "C:/Users/ci/.local/bin/ghapp"
    } else {
        "/Users/ci/.local/bin/ghapp"
    };
    const TEST_PRIVATE_KEY: &str = if cfg!(windows) {
        "C:/Users/ci/.config/shipyard/app.pem"
    } else {
        "/Users/ci/.config/shipyard/app.pem"
    };

    fn loaded_config(root: &Path, contents: &str) -> LoadedConfig {
        LoadedConfig {
            data: contents.parse::<Table>().expect("config TOML"),
            global_dir: root.join("global"),
            project_dir: Some(root.join(".shipyard")),
            local_dir: Some(root.join(".shipyard.local")),
            local_overlay_source: LocalOverlaySource::Direct,
        }
    }

    fn command_config(root: &Path, command: &[&str]) -> LoadedConfig {
        let mut auth = Table::new();
        auth.insert("source".to_owned(), TomlValue::String("command".to_owned()));
        auth.insert(
            "token_command".to_owned(),
            TomlValue::Array(
                command
                    .iter()
                    .map(|argument| TomlValue::String((*argument).to_owned()))
                    .collect(),
            ),
        );
        let mut github = Table::new();
        github.insert("auth".to_owned(), TomlValue::Table(auth));
        let mut data = Table::new();
        data.insert("github".to_owned(), TomlValue::Table(github));
        LoadedConfig {
            data,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    #[test]
    fn helper_argv_returns_only_typed_credential_arguments() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = TEST_WRAPPER;
        let config = command_config(
            temp.path(),
            &[
                wrapper,
                "token",
                "--app-id",
                "123456",
                "--private-key",
                TEST_PRIVATE_KEY,
                "--repo",
                "{repo_slug}",
            ],
        );

        let output = resolve_helper_argv(&config, Path::new(wrapper), "danielraffel/Shipyard")
            .expect("canonical resolver output");

        assert_eq!(output.schema_version, SCHEMA_VERSION);
        assert_eq!(output.command, "auth.helper-argv");
        assert_eq!(output.wrapper, wrapper);
        assert_eq!(output.repo, "danielraffel/Shipyard");
        assert_eq!(
            output.credential_argv,
            ["--app-id", "123456", "--private-key", TEST_PRIVATE_KEY,]
        );
    }

    #[test]
    fn helper_argv_rejects_every_noncanonical_token_command_shape() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = TEST_WRAPPER;
        let canonical = [
            wrapper,
            "token",
            "--app-id",
            "123456",
            "--private-key",
            TEST_PRIVATE_KEY,
            "--repo",
            "{repo_slug}",
        ];
        let cases = [
            (
                "foreign wrapper",
                [&["/tmp/foreign"][..], &canonical[1..]].concat(),
            ),
            ("missing argument", canonical[..7].to_vec()),
            (
                "duplicate argument",
                [canonical.as_slice(), &["--app-id", "999"]].concat(),
            ),
            (
                "api-url",
                [
                    canonical.as_slice(),
                    &["--api-url", "https://api.github.com"],
                ]
                .concat(),
            ),
            (
                "cache-dir",
                [canonical.as_slice(), &["--cache-dir", "/tmp/cache"]].concat(),
            ),
            (
                "installation-id",
                [canonical.as_slice(), &["--installation-id", "42"]].concat(),
            ),
            (
                "unknown",
                [canonical.as_slice(), &["--unknown", "value"]].concat(),
            ),
            (
                "non-placeholder repo",
                [&canonical[..7], &["danielraffel/Shipyard"]].concat(),
            ),
        ];

        for (name, command) in cases {
            let config = command_config(temp.path(), &command);
            let error = resolve_helper_argv(&config, Path::new(wrapper), "danielraffel/Shipyard")
                .expect_err(name);
            assert!(
                error.message.contains("must exactly match"),
                "{name}: {error:?}"
            );
        }
    }

    #[test]
    fn helper_argv_reads_only_machine_global_config() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = TEST_WRAPPER;
        let global = temp.path().join("global");
        let project = temp.path().join(".shipyard");
        fs::create_dir_all(&global).expect("global dir");
        fs::create_dir_all(&project).expect("project dir");
        fs::write(
            global.join("config.toml"),
            format!(
                r#"[github.auth]
source = "command"
token_command = ["{wrapper}", "token", "--app-id", "123456", "--private-key", "{TEST_PRIVATE_KEY}", "--repo", "{{repo_slug}}"]
"#
            ),
        )
        .expect("global config");
        fs::write(
            project.join("config.toml"),
            r#"[github.auth]
source = "command"
token_command = ["/tmp/foreign", "token", "--repo", "owner/foreign"]
"#,
        )
        .expect("project config");
        let mut stdout = Vec::new();

        auth_command(
            AuthCommand::HelperArgv {
                wrapper: PathBuf::from(wrapper),
                repo: "danielraffel/Shipyard".to_owned(),
            },
            RuntimeMode::Shipyard,
            temp.path(),
            &global,
            false,
            &mut stdout,
        )
        .expect("machine-global resolver");

        let output: JsonValue = serde_json::from_slice(&stdout).expect("typed JSON");
        assert_eq!(output["schema_version"], SCHEMA_VERSION);
        assert_eq!(output["credential_argv"][1], "123456");
    }

    #[test]
    fn helper_argv_rejects_malformed_routes() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = TEST_WRAPPER;
        let canonical = [
            wrapper,
            "token",
            "--app-id",
            "123456",
            "--private-key",
            TEST_PRIVATE_KEY,
            "--repo",
            "{repo_slug}",
        ];
        let config = command_config(temp.path(), &canonical);
        for repo in ["owner", "owner/repo/extra", "owner/..", "owner/repo name"] {
            assert!(
                resolve_helper_argv(&config, Path::new(wrapper), repo).is_err(),
                "accepted {repo:?}"
            );
        }
    }

    #[test]
    fn helper_argv_rejects_invalid_app_ids() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = TEST_WRAPPER;
        let oversized_app_id = "1".repeat(257);
        let oversized = [
            wrapper,
            "token",
            "--app-id",
            oversized_app_id.as_str(),
            "--private-key",
            TEST_PRIVATE_KEY,
            "--repo",
            "{repo_slug}",
        ];
        assert!(
            resolve_helper_argv(
                &command_config(temp.path(), &oversized),
                Path::new(wrapper),
                "owner/repo",
            )
            .is_err()
        );
        for app_id in [
            "0",
            "000",
            "not-numeric",
            "12x34",
            "１２３",
            "18446744073709551616",
        ] {
            let malformed = [
                wrapper,
                "token",
                "--app-id",
                app_id,
                "--private-key",
                TEST_PRIVATE_KEY,
                "--repo",
                "{repo_slug}",
            ];
            let error = resolve_helper_argv(
                &command_config(temp.path(), &malformed),
                Path::new(wrapper),
                "owner/repo",
            )
            .expect_err(app_id);
            assert!(error.message.contains("nonzero ASCII decimal"));
        }
    }

    #[test]
    fn helper_argv_requires_raw_normalized_paths() {
        let temp = TempDir::new().expect("tempdir");
        let wrapper = TEST_WRAPPER;
        let malformed_wrappers = if cfg!(windows) {
            [
                "C:/Users/ci//.local/bin/ghapp",
                "C:/Users/ci/./.local/bin/ghapp",
                "C:/Users/ci/../.local/bin/ghapp",
            ]
        } else {
            [
                "/Users/ci//.local/bin/ghapp",
                "/Users/ci/./.local/bin/ghapp",
                "/Users/ci/../.local/bin/ghapp",
            ]
        };
        for malformed_wrapper in malformed_wrappers {
            let malformed = [
                malformed_wrapper,
                "token",
                "--app-id",
                "123456",
                "--private-key",
                TEST_PRIVATE_KEY,
                "--repo",
                "{repo_slug}",
            ];
            assert!(
                resolve_helper_argv(
                    &command_config(temp.path(), &malformed),
                    Path::new(malformed_wrapper),
                    "owner/repo",
                )
                .is_err()
            );
        }
        let malformed_private_keys = if cfg!(windows) {
            [
                "C:/Users/ci//app.pem",
                "C:/Users/ci/./app.pem",
                "C:/Users/ci/../app.pem",
            ]
        } else {
            [
                "/Users/ci//app.pem",
                "/Users/ci/./app.pem",
                "/Users/ci/../app.pem",
            ]
        };
        for private_key in malformed_private_keys {
            let malformed = [
                wrapper,
                "token",
                "--app-id",
                "123456",
                "--private-key",
                private_key,
                "--repo",
                "{repo_slug}",
            ];
            assert!(
                resolve_helper_argv(
                    &command_config(temp.path(), &malformed),
                    Path::new(wrapper),
                    "owner/repo",
                )
                .is_err()
            );
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
    fn export_bundle_preserves_ambient_gh_binary_requirement() {
        let temp = TempDir::new().expect("tempdir");
        let binary = if cfg!(windows) {
            "C:/Program Files/GitHub CLI/gh.exe"
        } else {
            "/usr/local/bin/gh"
        };
        let config = loaded_config(
            temp.path(),
            &format!(
                r#"
            [github.auth]
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            ambient_gh_binary = "{binary}"
            "#
            ),
        );

        let bundle = export_bundle(&config);
        let requirements = bundle
            .get("requirements")
            .and_then(TomlValue::as_table)
            .expect("requirements");

        assert!(
            requirements
                .get("commands")
                .and_then(TomlValue::as_array)
                .is_some_and(|commands| commands
                    .iter()
                    .any(|value| value.as_str() == Some(binary)))
        );
        assert!(
            requirements
                .get("notes")
                .and_then(TomlValue::as_array)
                .is_some_and(|notes| notes.iter().any(|value| value
                    .as_str()
                    .is_some_and(|note| note.contains("scripts and wrappers are rejected"))))
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

    fn assert_machine_auth_bundle_round_trip(
        root: &Path,
        machine: &str,
        binary_authority: &str,
        stale_key: &str,
        stale_setting: &str,
    ) {
        let source = loaded_config(
            root,
            &format!(
                r#"
                [github.auth]
                source = "command"
                token_command = ["{TEST_WRAPPER}", "token", "--app-id", "123456", "--private-key", "{TEST_PRIVATE_KEY}", "--repo", "{{repo_slug}}"]
                refresh_skew_seconds = 60
                {binary_authority}
                "#,
            ),
        );
        let bundle = export_bundle(&source);
        let input = root.join(format!("{machine}-auth.toml"));
        fs::write(&input, format!("{bundle}\n")).expect("bundle");
        let global_dir = root.join(format!("{machine}-global"));
        fs::create_dir_all(&global_dir).expect("global dir");
        fs::write(
            global_dir.join("config.toml"),
            format!(
                r#"
                [rollout_audit]
                machine = "{machine}"

                [github.auth]
                source = "command"
                token_command = ["{TEST_WRAPPER}"]
                {stale_setting}
                "#,
            ),
        )
        .expect("preexisting global config");
        let mut output = Vec::new();

        auth_import(
            RuntimeMode::Shipyard,
            root,
            &global_dir,
            &source,
            &input,
            AuthConfigScope::Global,
            false,
            &mut output,
        )
        .unwrap_or_else(|error| panic!("{machine} import failed: {}", error.message));

        let imported = fs::read_to_string(global_dir.join("config.toml"))
            .expect("imported global config")
            .parse::<Table>()
            .expect("imported TOML");
        let imported_auth = imported
            .get("github")
            .and_then(TomlValue::as_table)
            .and_then(|github| github.get("auth"))
            .and_then(TomlValue::as_table);
        let bundled_auth = bundle
            .get("github")
            .and_then(TomlValue::as_table)
            .and_then(|github| github.get("auth"))
            .and_then(TomlValue::as_table);
        assert_eq!(
            imported_auth, bundled_auth,
            "{machine} auth changed during export/import",
        );
        assert_eq!(
            imported_auth.and_then(|auth| auth.get(stale_key)),
            None,
            "{machine} retained stale auth key {stale_key}",
        );
        assert_eq!(
            imported
                .get("rollout_audit")
                .and_then(TomlValue::as_table)
                .and_then(|audit| audit.get("machine"))
                .and_then(TomlValue::as_str),
            Some(machine),
            "{machine} import changed unrelated global config",
        );
    }

    #[test]
    fn exported_machine_auth_bundles_round_trip_privileged_binary_authority() {
        let temp = TempDir::new().expect("tempdir");
        let gh = if cfg!(windows) {
            "C:/Program Files/GitHub CLI/gh.exe"
        } else {
            "/opt/homebrew/bin/gh"
        };
        let git = if cfg!(windows) {
            "C:/Program Files/Git/cmd/git.exe"
        } else {
            "/usr/bin/git"
        };
        let cases = [
            (
                "m5",
                format!(
                    r#"
                    ambient_gh_binary = "{gh}"
                    privileged_gh_binary = "{gh}"
                    privileged_git_binary = "{git}"
                    "#,
                ),
                "cache_ttl_seconds",
                "cache_ttl_seconds = 300".to_owned(),
            ),
            (
                "m1",
                format!(
                    r#"
                    privileged_git_binary = "{git}"
                    "#,
                ),
                "ambient_gh_binary",
                format!("ambient_gh_binary = \"{gh}\""),
            ),
            (
                "m3",
                format!(
                    r#"
                    privileged_gh_binary = "{gh}"
                    privileged_git_binary = "{git}"
                    "#,
                ),
                "ambient_gh_binary",
                format!("ambient_gh_binary = \"{gh}\""),
            ),
        ];

        for (machine, binary_authority, stale_key, stale_setting) in cases {
            assert_machine_auth_bundle_round_trip(
                temp.path(),
                machine,
                &binary_authority,
                stale_key,
                &stale_setting,
            );
        }
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
