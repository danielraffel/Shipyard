//! `shipyard update` — self-update from the CLI.
//!
//! Phase 1 (this module):
//! - `--check` / `--json` query GitHub's REST `releases/latest` (no GraphQL,
//!   matches the policy from #289) and report installed-vs-available.
//! - `--to vX.Y.Z` pins to a specific tag.
//! - Apply path delegates to `install.sh` so we don't reimplement
//!   platform-specific dmg-mount / atomic-rename / Windows .cmd shimming.
//!   That's the canonical bootstrap path; Phase 2 will move it native.

use std::collections::BTreeMap;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Duration;

use serde_json::Value;

use super::{CliFailure, cli::UpdateArgs};
use crate::config::LoadedConfig;
use crate::gh::GhClient;
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::paths::{RuntimePaths, home_dir, unattended_tool_path};

const UPDATE_EVENT: &str = "update";
const DEFAULT_RELEASES_API_BASE: &str =
    "https://api.github.com/repos/danielraffel/Shipyard/releases";
const UPDATE_AUTH_TIMEOUT: Duration = Duration::from_secs(15);

/// CLI dispatch entry.
pub(super) fn update_command<W: Write>(
    args: &UpdateArgs,
    mode: RuntimeMode,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let installed = installed_version();
    // Self-update is machine policy, so only the trusted machine-global layer
    // may select credentials or executable paths. A configured auth source is
    // authoritative and fail-closed; only an unconfigured/ambient installation
    // retains the public-repo unauthenticated fallback.
    let config = LoadedConfig::load_machine_global_from_dir(runtime_paths.global_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if args.unattended_fleet {
        validate_unattended_auth(&config)?;
    }
    let explicit_target = args.to.as_deref().filter(|value| !value.trim().is_empty());
    let (target, mut token, staged_curl) = if let Some(raw) = explicit_target {
        (
            normalize_tag(raw).map_err(|message| CliFailure::new(2, message))?,
            None,
            None,
        )
    } else {
        let token =
            discover_github_token(&config, cwd).map_err(|message| CliFailure::new(1, message))?;
        let curl = resolve_update_tool(args.curl_bin.as_deref(), &config, "curl_bin", "curl")?;
        let target = fetch_latest_tag(
            args.releases_api_base
                .as_deref()
                .unwrap_or(DEFAULT_RELEASES_API_BASE),
            &curl,
            token.as_deref(),
        )
        .map_err(|message| CliFailure::new(1, message))?;
        (target, token, Some(curl))
    };
    if args.unattended_fleet && token.is_none() {
        token =
            discover_github_token(&config, cwd).map_err(|message| CliFailure::new(1, message))?;
    }
    let update_available = target_is_newer(&installed, &target);

    if args.check {
        return render_check(
            &installed,
            &target,
            update_available,
            args.dry_run,
            json,
            stdout,
        );
    }
    if args.dry_run {
        return render_plan(&installed, &target, update_available, json, stdout);
    }

    if explicit_target.is_some() && token.is_none() {
        token =
            discover_github_token(&config, cwd).map_err(|message| CliFailure::new(1, message))?;
    }
    let curl_bin = match staged_curl {
        Some(curl) => curl,
        None => resolve_update_tool(args.curl_bin.as_deref(), &config, "curl_bin", "curl")?,
    };
    let shell_bin = resolve_update_tool(args.shell_bin.as_deref(), &config, "shell_bin", "bash")?;
    let current_binary = std::env::current_exe()
        .map_err(|error| CliFailure::new(1, format!("failed to locate current binary: {error}")))?;
    let configured_install_dir = std::env::var_os("SHIPYARD_INSTALL_DIR").map(PathBuf::from);
    let install_dir = update_install_dir(
        args.unattended_fleet,
        &current_binary,
        configured_install_dir.as_deref(),
    )?;
    let installed_binary = install_dir.join(format!("shipyard{}", std::env::consts::EXE_SUFFIX));
    let tools = UpdateTools {
        token: token.as_deref(),
        curl_bin: &curl_bin,
        shell_bin: &shell_bin,
        install_dir: &install_dir,
    };
    let applied = apply_update(
        args,
        &installed,
        &target,
        update_available,
        &tools,
        json,
        stdout,
    )?;
    if !applied {
        return Ok(ExitCode::SUCCESS);
    }
    verify_installed_version(&installed_binary, &target)?;

    if args.refresh_daemon {
        let pid = refresh_daemon_with_installed_binary(mode, runtime_paths, &installed_binary)
            .map_err(|message| {
                CliFailure::new(
                    3,
                    format!("update installed, but daemon refresh failed: {message}"),
                )
            })?;
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("daemon_refreshed"));
        data.insert("target".to_owned(), Value::from(target));
        data.insert("daemon_pid".to_owned(), Value::from(pid));
        render(stdout, json, data, || {
            format!("daemon refreshed (pid {pid}).")
        })?;
    }
    Ok(ExitCode::SUCCESS)
}

fn update_install_dir(
    unattended_fleet: bool,
    current_binary: &Path,
    configured_install_dir: Option<&Path>,
) -> Result<PathBuf, CliFailure> {
    if unattended_fleet {
        return current_binary
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| CliFailure::new(1, "current binary has no install directory"));
    }
    if let Some(configured) = configured_install_dir {
        if !configured.is_absolute() {
            return Err(CliFailure::new(
                2,
                "SHIPYARD_INSTALL_DIR must be an absolute path",
            ));
        }
        return Ok(configured.to_path_buf());
    }
    // Ordinary self-update retains install.sh's canonical destination even
    // when a source-built or PATH-shadowing Shipyard invoked the command.
    Ok(home_dir().join(".local/bin"))
}

fn verify_installed_version(binary: &Path, target_tag: &str) -> Result<(), CliFailure> {
    verify_installed_version_with_command(&mut Command::new(binary), binary, target_tag)
}

fn verify_installed_version_with_command(
    command: &mut Command,
    binary: &Path,
    target_tag: &str,
) -> Result<(), CliFailure> {
    let expected = format!("shipyard {}", target_tag.trim_start_matches('v'));
    let output = command
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            CliFailure::new(
                1,
                format!(
                    "failed to verify installed binary {}: {error}",
                    binary.display()
                ),
            )
        })?;
    let actual = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if output.status.success() && actual == expected {
        return Ok(());
    }
    Err(CliFailure::new(
        1,
        format!(
            "installed binary verification mismatch: expected {expected:?}, observed {actual:?}; daemon was not refreshed"
        ),
    ))
}

/// Cross the self-update process boundary before refreshing the daemon. The
/// process that performed the install may predate daemon-spawn fixes in the
/// release it just installed, so it must not execute its own refresh code.
fn refresh_daemon_with_installed_binary(
    mode: RuntimeMode,
    runtime_paths: &RuntimePaths,
    installed_binary: &Path,
) -> Result<u32, String> {
    let output = Command::new(installed_binary)
        .arg("--mode")
        .arg(mode.as_str())
        .arg("--global-dir")
        .arg(&runtime_paths.global_dir)
        .arg("--state-dir")
        .arg(&runtime_paths.state_dir)
        .arg("--json")
        .args(["daemon", "refresh"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "failed to execute verified installed binary {}: {error}",
                installed_binary.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "verified installed binary {} exited {} while refreshing the daemon",
            installed_binary.display(),
            output.status.code().unwrap_or(-1)
        ));
    }
    let receipt: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        "verified installed binary returned an invalid daemon refresh receipt".to_owned()
    })?;
    if receipt.get("schema_version").and_then(Value::as_u64)
        != Some(u64::from(crate::output::SCHEMA_VERSION))
        || receipt.get("command").and_then(Value::as_str) != Some("daemon:refresh")
    {
        return Err(
            "verified installed binary returned an unexpected daemon refresh receipt".to_owned(),
        );
    }
    receipt
        .get("new_pid")
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| {
            "verified installed binary did not prove the refreshed daemon PID".to_owned()
        })
}

fn validate_unattended_auth(config: &LoadedConfig) -> Result<(), CliFailure> {
    if config.get_str("github.auth.source") == Some("command") {
        return Ok(());
    }
    Err(CliFailure::new(
        1,
        "unattended fleet update requires machine-global github.auth.source = \"command\"; env and ambient auth are intentionally unavailable under the stripped launch environment",
    ))
}

fn installed_version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

/// Discover a GitHub token for read-only release queries. A configured
/// machine-global source is authoritative and errors fail closed. Only an
/// ambient configuration may fall back through env, absolute `gh`, and finally
/// the public repository's unauthenticated API.
fn discover_github_token(config: &LoadedConfig, cwd: &Path) -> Result<Option<String>, String> {
    let configured = GhClient::from_loaded_config(config)
        .map_err(|error| format!("failed to load governed GitHub auth: {error}"))?
        .with_repo_override("danielraffel/Shipyard")
        .map_err(|error| format!("failed to bind governed GitHub auth: {error}"))?
        .resolve_token_for_child(cwd, UPDATE_AUTH_TIMEOUT)
        .map_err(|error| format!("failed to resolve governed GitHub auth: {error}"))?;
    Ok(configured
        .or_else(|| select_env_token(|name| std::env::var(name).ok()))
        .or_else(gh_cli_token))
}

/// Pure precedence over the recognized token env vars, factored for testing.
fn select_env_token<F: Fn(&str) -> Option<String>>(get: F) -> Option<String> {
    for name in ["SHIPYARD_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Some(value) = get(name) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_owned());
            }
        }
    }
    None
}

/// Best-effort `gh auth token`. Returns `None` if `gh` is absent, not logged
/// in, or emits nothing — callers degrade to unauthenticated.
fn gh_cli_token() -> Option<String> {
    let gh = default_tool_path("gh")?;
    let output = Command::new(gh).args(["auth", "token"]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if token.is_empty() { None } else { Some(token) }
}

fn resolve_update_tool(
    cli_override: Option<&Path>,
    config: &LoadedConfig,
    config_key: &str,
    program: &str,
) -> Result<PathBuf, CliFailure> {
    if let Some(path) = cli_override {
        if !path.is_absolute() {
            let flag = config_key.replace('_', "-");
            return Err(CliFailure::new(
                1,
                format!("--{flag} must be an absolute path"),
            ));
        }
        return Ok(path.to_path_buf());
    }
    if let Some(configured) = config.get_str(&format!("update.{config_key}")) {
        let path = PathBuf::from(configured);
        if !path.is_absolute() {
            return Err(CliFailure::new(
                1,
                format!("update.{config_key} must be an absolute path"),
            ));
        }
        return Ok(path);
    }
    default_tool_path(program).ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "could not resolve {program} from canonical unattended paths; configure update.{config_key} with an absolute path"
            ),
        )
    })
}

fn default_tool_path(program: &str) -> Option<PathBuf> {
    canonical_tool_candidates(program)
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn canonical_tool_candidates(program: &str) -> Vec<PathBuf> {
    #[cfg(unix)]
    return match program {
        // Prefer OS-owned clients. User-prefix curl/bash entries may be shell
        // wrappers that inject a different GitHub identity.
        "curl" => vec![PathBuf::from("/usr/bin/curl"), PathBuf::from("/bin/curl")],
        "bash" => vec![PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")],
        "gh" => vec![
            PathBuf::from("/opt/homebrew/bin/gh"),
            PathBuf::from("/usr/local/bin/gh"),
            home_dir().join(".local/bin/gh"),
        ],
        _ => Vec::new(),
    };
    #[cfg(windows)]
    {
        let system_root = std::env::var_os("SystemRoot").map(PathBuf::from);
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
        let local_app_data = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
        match program {
            "curl" => system_root
                .into_iter()
                .map(|root| root.join("System32/curl.exe"))
                .collect(),
            "bash" => program_files
                .into_iter()
                .flat_map(|root| {
                    [
                        root.join("Git/bin/bash.exe"),
                        root.join("Git/usr/bin/bash.exe"),
                    ]
                })
                .chain(
                    local_app_data
                        .into_iter()
                        .map(|root| root.join("Programs/Git/bin/bash.exe")),
                )
                .collect(),
            "gh" => program_files
                .into_iter()
                .map(|root| root.join("GitHub CLI/gh.exe"))
                .collect(),
            _ => Vec::new(),
        }
    }
    #[cfg(not(any(unix, windows)))]
    Vec::new()
}

fn normalize_tag(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    let version = trimmed.strip_prefix('v').unwrap_or(trimmed);
    let parts = version.split('.').collect::<Vec<_>>();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.chars().all(|ch| ch.is_ascii_digit()))
    {
        return Err("update --to requires an exact stable vMAJOR.MINOR.PATCH tag".to_owned());
    }
    Ok(format!("v{version}"))
}

fn fetch_latest_tag(
    api_base: &str,
    curl_bin: &Path,
    token: Option<&str>,
) -> Result<String, String> {
    let url = format!("{api_base}/latest");
    // `-sS` (not `-f`) so an HTTP error still yields the response body, and
    // `-w \n%{http_code}` appends the final status — together they let us tell
    // a 403 rate-limit apart from a genuinely missing release.
    let mut command = Command::new(curl_bin);
    command.args([
        "-sSL",
        "-w",
        "\n%{http_code}",
        "-H",
        "Accept: application/vnd.github+json",
        "-H",
        "User-Agent: shipyard-update",
    ]);
    if let Some(token) = token {
        let mut config = tempfile::tempfile()
            .map_err(|error| format!("failed to stage curl auth config: {error}"))?;
        let escaped = token.replace('\\', "\\\\").replace('"', "\\\"");
        writeln!(config, "header = \"Authorization: Bearer {escaped}\"")
            .and_then(|()| config.seek(SeekFrom::Start(0)))
            .map_err(|error| format!("failed to stage curl auth config: {error}"))?;
        command.args(["--config", "-"]).stdin(Stdio::from(config));
    }
    command.arg(&url);
    let output = command
        .output()
        .map_err(|error| format!("failed to invoke curl: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(format!(
            "GitHub releases/latest request failed: {}",
            if stderr.is_empty() {
                "curl exited non-zero".to_owned()
            } else {
                stderr
            }
        ));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    let (body, http_code) = split_trailing_http_code(&raw);
    classify_release_response(http_code, body, token.is_some())
}

/// Split curl's trailing `%{http_code}` (appended after a final newline) from
/// the response body. The body may itself contain newlines (pretty-printed
/// JSON), so we split on the *last* newline only.
fn split_trailing_http_code(raw: &str) -> (&str, &str) {
    match raw.rsplit_once('\n') {
        Some((body, code)) => (body, code.trim()),
        None => (raw, ""),
    }
}

/// Turn an HTTP status + body into a tag name or a precise error. A 403/429
/// carrying a rate-limit body becomes an actionable message rather than the
/// misleading "no binary found" the installer would otherwise surface.
fn classify_release_response(
    http_code: &str,
    body: &str,
    authenticated: bool,
) -> Result<String, String> {
    if matches!(http_code, "403" | "429")
        && (body.contains("rate limit") || body.contains("API rate limit"))
    {
        return Err(rate_limit_message(authenticated));
    }
    if !http_code.is_empty() && !http_code.starts_with('2') {
        let detail = body.lines().next().unwrap_or("").trim();
        return Err(format!(
            "GitHub releases/latest returned HTTP {http_code}{}",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        ));
    }
    parse_tag_name(body)
}

fn rate_limit_message(authenticated: bool) -> String {
    if authenticated {
        "GitHub API rate limit exceeded even with an authenticated token. Wait for the rate-limit \
         window to reset, then retry `shipyard update`."
            .to_owned()
    } else {
        "GitHub API rate limit exceeded (unauthenticated requests are capped at 60/hr). Set \
         GITHUB_TOKEN or run `gh auth login` to raise the limit to 5000/hr, then retry `shipyard \
         update`."
            .to_owned()
    }
}

fn parse_tag_name(body: &str) -> Result<String, String> {
    let value = serde_json::from_str::<Value>(body)
        .map_err(|error| format!("failed to parse releases JSON: {error}"))?;
    let tag = value
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or_else(|| "releases JSON missing `tag_name`".to_owned())?;
    Ok(tag.to_owned())
}

fn target_is_newer(installed: &str, target_tag: &str) -> bool {
    let target = target_tag.strip_prefix('v').unwrap_or(target_tag);
    compare_semver(installed, target).is_lt()
}

fn compare_semver(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |raw: &str| -> [u64; 3] {
        let mut parts = [0u64; 3];
        for (idx, segment) in raw.split('.').take(3).enumerate() {
            let cleaned: String = segment.chars().take_while(char::is_ascii_digit).collect();
            parts[idx] = cleaned.parse::<u64>().unwrap_or(0);
        }
        parts
    };
    parse(a).cmp(&parse(b))
}

fn render_check<W: Write>(
    installed: &str,
    target_tag: &str,
    update_available: bool,
    dry_run: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("check"));
    data.insert("installed".to_owned(), Value::from(installed.to_owned()));
    data.insert("target".to_owned(), Value::from(target_tag.to_owned()));
    data.insert("update_available".to_owned(), Value::Bool(update_available));
    data.insert("dry_run".to_owned(), Value::Bool(dry_run));
    render(stdout, json, data, || {
        if update_available {
            format!(
                "installed={installed} available={target_tag} → update available (run `shipyard update` to apply)."
            )
        } else {
            format!("installed={installed} target={target_tag} → already up to date.")
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

fn render_plan<W: Write>(
    installed: &str,
    target_tag: &str,
    update_available: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("plan"));
    data.insert("installed".to_owned(), Value::from(installed.to_owned()));
    data.insert("target".to_owned(), Value::from(target_tag.to_owned()));
    data.insert("update_available".to_owned(), Value::Bool(update_available));
    data.insert("dry_run".to_owned(), Value::Bool(true));
    render(stdout, json, data, || {
        if update_available {
            format!(
                "Dry-run: would install {target_tag} (current: {installed}) via install.sh. Re-run without --dry-run to apply."
            )
        } else {
            format!("Dry-run: installed={installed} matches target={target_tag}; nothing to do.")
        }
    })?;
    Ok(ExitCode::SUCCESS)
}

struct UpdateTools<'a> {
    token: Option<&'a str>,
    curl_bin: &'a Path,
    shell_bin: &'a Path,
    install_dir: &'a Path,
}

fn apply_update<W: Write>(
    args: &UpdateArgs,
    installed: &str,
    target_tag: &str,
    update_available: bool,
    tools: &UpdateTools<'_>,
    json: bool,
    stdout: &mut W,
) -> Result<bool, CliFailure> {
    if !update_available && args.to.is_none() {
        // No-op fast path; --to forces install even if equal/older.
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("noop"));
        data.insert("installed".to_owned(), Value::from(installed.to_owned()));
        data.insert("target".to_owned(), Value::from(target_tag.to_owned()));
        render(stdout, json, data, || {
            format!("installed={installed} already matches target={target_tag}; no update applied.")
        })?;
        return Ok(false);
    }

    let tagged_install_script_url =
        format!("https://raw.githubusercontent.com/danielraffel/Shipyard/{target_tag}/install.sh");
    let install_script_url = args
        .install_script_url
        .as_deref()
        .unwrap_or(&tagged_install_script_url);

    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("apply"));
    data.insert("installed".to_owned(), Value::from(installed.to_owned()));
    data.insert("target".to_owned(), Value::from(target_tag.to_owned()));
    data.insert(
        "install_script".to_owned(),
        Value::from(install_script_url.to_owned()),
    );
    render(stdout, json, data, || {
        format!("Updating from {installed} to {target_tag} via {install_script_url} …")
    })?;

    invoke_install_script(
        tools.curl_bin,
        tools.shell_bin,
        install_script_url,
        target_tag,
        tools.token,
        tools.install_dir,
        json,
    )?;

    let mut data = BTreeMap::new();
    data.insert("event".to_owned(), Value::from("applied"));
    data.insert("installed".to_owned(), Value::from(installed.to_owned()));
    data.insert("target".to_owned(), Value::from(target_tag.to_owned()));
    render(stdout, json, data, || {
        format!(
            "Update to {target_tag} applied. Run `shipyard --version` to confirm the new binary is on PATH."
        )
    })?;
    Ok(true)
}

fn invoke_install_script(
    curl_bin: &Path,
    shell_bin: &Path,
    install_script_url: &str,
    target_tag: &str,
    token: Option<&str>,
    install_dir: &Path,
    json: bool,
) -> Result<(), CliFailure> {
    invoke_install_script_with_commands(
        InstallerCommands {
            curl_bin,
            curl: Command::new(curl_bin),
            shell: Command::new(shell_bin),
        },
        install_script_url,
        target_tag,
        token,
        install_dir,
        json,
    )
}

struct InstallerCommands<'a> {
    curl_bin: &'a Path,
    curl: Command,
    shell: Command,
}

fn invoke_install_script_with_commands(
    commands: InstallerCommands<'_>,
    install_script_url: &str,
    target_tag: &str,
    token: Option<&str>,
    install_dir: &Path,
    json: bool,
) -> Result<(), CliFailure> {
    let InstallerCommands {
        curl_bin,
        mut curl,
        mut shell,
    } = commands;
    // Fetch the entire tagged installer before executing any of it. Streaming
    // curl into a shell allowed a truncated response to run far enough to
    // mutate the install before curl's failure was known.
    let installer = tempfile::NamedTempFile::new()
        .map_err(|error| CliFailure::new(1, format!("failed to stage installer: {error}")))?;
    let curl_status = curl
        .args(["-fsSL", "--output"])
        .arg(installer.path())
        .arg(install_script_url)
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| CliFailure::new(1, format!("failed to spawn curl: {error}")))?;
    if !curl_status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "curl exited {} while fetching {install_script_url}; installer was not executed",
                curl_status.code().unwrap_or(-1)
            ),
        ));
    }
    if installer
        .as_file()
        .metadata()
        .map_or(true, |meta| meta.len() == 0)
    {
        return Err(CliFailure::new(
            1,
            "downloaded installer was empty; installer was not executed",
        ));
    }

    // Under `--json`, route installer progress to stderr so the stdout
    // stream stays a clean sequence of JSON envelopes for downstream
    // automation. In human mode, keep the installer's stdout visible so
    // the user sees the progress bar.
    let install_stdout = if json {
        // Inherit our own stderr — install.sh's stdout becomes our stderr.
        // `Stdio::from(std::io::stderr())` would consume our stderr handle;
        // duplicate the parent stderr fd instead.
        Stdio::from(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/stderr")
                .map_err(|error| {
                    CliFailure::new(1, format!("failed to open /dev/stderr: {error}"))
                })?,
        )
    } else {
        Stdio::inherit()
    };

    // The installer is an external process, so establish the writer domain
    // before handing ownership to its self-owning guardian below. The network
    // fetch above remains outside the critical section.
    let writer_domain = crate::writer_domain_lease::acquire_for_protected_path(install_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    let env_tag = target_tag.strip_prefix('v').unwrap_or(target_tag);
    shell
        .arg(installer.path())
        .env("PATH", unattended_tool_path())
        .env("SHIPYARD_VERSION", env_tag)
        .env("SHIPYARD_INSTALL_DIR", install_dir)
        .env("SHIPYARD_CURL_BIN", curl_bin);
    // install.sh reads SHIPYARD_GITHUB_TOKEN/GITHUB_TOKEN for its own release
    // lookup; pass the discovered token through so its API calls are also
    // authenticated (and not rate-limited) when one is available.
    if let Some(token) = token {
        shell.env("SHIPYARD_GITHUB_TOKEN", token);
    }
    let mut guarded;
    let command = if writer_domain.is_some() {
        guarded = crate::writer_domain_lease::guarded_child_command(&shell, install_dir).map_err(
            |error| CliFailure::new(1, format!("failed to prepare installer guardian: {error}")),
        )?;
        &mut guarded
    } else {
        &mut shell
    };
    // The guardian owns the child transaction directly. Release the parent
    // lease before it attempts acquisition so an arriving exclusive audit can
    // interpose without forming a parent/child/turnstile wait cycle.
    drop(writer_domain);
    let mut sh = command
        .stdin(Stdio::null())
        .stdout(install_stdout)
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| CliFailure::new(1, format!("failed to spawn shell: {error}")))?;

    let sh_status = sh
        .wait()
        .map_err(|error| CliFailure::new(1, format!("install.sh wait failed: {error}")))?;
    if !sh_status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "install.sh exited {}; binary may not have been replaced",
                sh_status.code().unwrap_or(-1)
            ),
        ));
    }
    Ok(())
}

fn render<W: Write>(
    stdout: &mut W,
    json: bool,
    data: BTreeMap<String, Value>,
    human: impl FnOnce() -> String,
) -> Result<(), CliFailure> {
    if json {
        write_json_envelope(stdout, UPDATE_EVENT, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(());
    }
    writeln!(stdout, "{}", human()).map_err(|error| CliFailure::new(1, error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Argument that makes every fixture written here exit immediately, so the
    /// readiness probe below cannot trigger a fixture's real behaviour.
    #[cfg(unix)]
    const PROBE_ARG: &str = "--shipyard-fixture-probe";

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        // Fixtures short-circuit on the probe argument. Without this the probe
        // would run the script for real, and a fixture with side effects would
        // record an invocation nobody made.
        let guarded = contents.replacen(
            "#!/bin/sh\n",
            &format!("#!/bin/sh\ncase \"${{1:-}}\" in {PROBE_ARG}) exit 0;; esac\n"),
            1,
        );
        // If a future fixture uses a different shebang the guard silently will
        // not apply, and the probe below would run it for real. Fail loudly
        // rather than let that become an invocation nobody made.
        assert!(
            guarded.contains(PROBE_ARG),
            "a fixture must begin with `#!/bin/sh` so the readiness probe cannot execute it: {contents}"
        );
        std::fs::write(path, &guarded).expect("write executable");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(path, permissions).expect("permissions");
        wait_until_executable(path);
    }

    /// Wait until the fixture can actually be executed.
    ///
    /// A freshly written file can fail `exec` with `ETXTBSY` ("Text file
    /// busy", errno 26) while any process still holds it open for writing.
    /// `fs::write` closes its own handle, but this is a multi-threaded test
    /// binary: a sibling test that forks between the `write` and the `exec`
    /// leaves the child holding an inherited descriptor, and the exec fails.
    ///
    /// Renaming a staged file into place does **not** avoid this — the
    /// descriptor refers to the inode, not the path. Only observing that the
    /// file runs proves it is no longer busy.
    ///
    /// Linux enforces `ETXTBSY`; macOS does not, so this failure is invisible
    /// locally and only ever appears on the Linux leg.
    #[cfg(unix)]
    fn wait_until_executable(path: &Path) {
        for _ in 0..200 {
            let busy = std::process::Command::new(path)
                .arg(PROBE_ARG)
                .output()
                .err()
                .is_some_and(|error| error.raw_os_error() == Some(26));
            if !busy {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("fixture never became executable: {}", path.display());
    }

    #[cfg(unix)]
    #[test]
    fn governed_absolute_token_helper_works_without_ambient_cli_lookup() {
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = temp.path().join("token-helper");
        write_executable(&helper, "#!/bin/sh\nprintf governed-test-token\n");
        std::fs::write(
            temp.path().join("config.toml"),
            format!(
                "[github.auth]\nsource = \"command\"\ntoken_command = [{}]\n",
                toml::Value::String(helper.display().to_string())
            ),
        )
        .expect("config");
        let config = LoadedConfig::load_machine_global_from_dir(temp.path().to_path_buf())
            .expect("load config");

        assert_eq!(
            discover_github_token(&config, temp.path()).expect("resolve"),
            Some("governed-test-token".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn governed_token_helper_failure_is_fail_closed_and_redacted() {
        let temp = tempfile::tempdir().expect("tempdir");
        let helper = temp.path().join("token-helper");
        write_executable(
            &helper,
            "#!/bin/sh\nprintf 'helper failed with ghp_should_not_leak' >&2\nexit 7\n",
        );
        std::fs::write(
            temp.path().join("config.toml"),
            format!(
                "[github.auth]\nsource = \"command\"\ntoken_command = [{}]\n",
                toml::Value::String(helper.display().to_string())
            ),
        )
        .expect("config");
        let config = LoadedConfig::load_machine_global_from_dir(temp.path().to_path_buf())
            .expect("load config");

        let error = discover_github_token(&config, temp.path()).expect_err("helper must fail");
        assert!(error.contains("failed to resolve governed GitHub auth"));
        assert!(!error.contains("ghp_should_not_leak"));
    }

    #[cfg(unix)]
    #[test]
    fn release_query_keeps_governed_token_out_of_process_arguments() {
        let temp = tempfile::tempdir().expect("tempdir");
        let curl = temp.path().join("curl");
        let args_capture = temp.path().join("args");
        let stdin = temp.path().join("stdin");
        write_executable(
            &curl,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\ncat > {}\nprintf '{{\"tag_name\":\"v0.98.1\"}}\\n200'\n",
                shlex_quote_path(&args_capture),
                shlex_quote_path(&stdin),
            ),
        );

        assert_eq!(
            fetch_latest_tag(
                "https://example.invalid/releases",
                &curl,
                Some("ghp_governed_secret"),
            )
            .expect("release tag"),
            "v0.98.1"
        );
        let argv = std::fs::read_to_string(args_capture).expect("argv capture");
        let curl_config = std::fs::read_to_string(stdin).expect("stdin capture");
        assert!(!argv.contains("ghp_governed_secret"));
        assert!(argv.contains("--config\n-"));
        assert!(curl_config.contains("Authorization: Bearer ghp_governed_secret"));
    }

    #[cfg(unix)]
    #[test]
    fn failed_installer_download_never_executes_partial_script() {
        let temp = tempfile::tempdir().expect("tempdir");
        let curl_fixture = temp.path().join("curl-fixture.sh");
        let shell_fixture = temp.path().join("shell-fixture.sh");
        let marker = temp.path().join("shell-ran");
        std::fs::write(
            &curl_fixture,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--output\" ]; then shift; printf partial > \"$1\"; fi\n  shift\ndone\nexit 22\n",
        )
        .expect("write curl fixture");
        std::fs::write(
            &shell_fixture,
            format!("#!/bin/sh\n/usr/bin/touch {}\n", shlex_quote_path(&marker)),
        )
        .expect("write shell fixture");
        let mut curl_command = Command::new("/bin/sh");
        curl_command.arg(&curl_fixture);
        let mut shell_command = Command::new("/bin/sh");
        shell_command.arg(&shell_fixture);

        let error = invoke_install_script_with_commands(
            InstallerCommands {
                curl_bin: Path::new("/bin/sh"),
                curl: curl_command,
                shell: shell_command,
            },
            "https://example.invalid/install.sh",
            "v0.98.1",
            Some("governed-test-token"),
            temp.path(),
            false,
        )
        .expect_err("curl failure");

        assert!(
            !error.message.contains("governed-test-token"),
            "failed download exposed the governed token"
        );
        assert!(
            error.message.contains("curl exited 22")
                && error.message.contains("installer was not executed"),
            "failed download did not report the expected safe refusal: {}",
            error.message
        );
        assert!(
            !marker.exists(),
            "shell fixture ran after a partial installer download: {}",
            marker.display()
        );
    }

    #[cfg(unix)]
    #[test]
    fn installer_receives_exact_binary_directory_and_curl_identity() {
        let temp = tempfile::tempdir().expect("tempdir");
        let curl = temp.path().join("custom-curl");
        let shell = temp.path().join("shell");
        let marker = temp.path().join("installer-env");
        let install_dir = temp.path().join("custom install");
        std::fs::create_dir(&install_dir).expect("install dir");
        write_executable(
            &curl,
            "#!/bin/sh\nwhile [ \"$#\" -gt 0 ]; do\n  if [ \"$1\" = \"--output\" ]; then shift; printf '#!/bin/sh\\n' > \"$1\"; fi\n  shift\ndone\n",
        );
        write_executable(
            &shell,
            &format!(
                "#!/bin/sh\nprintf '%s\\n%s\\n' \"$SHIPYARD_INSTALL_DIR\" \"$SHIPYARD_CURL_BIN\" > {}\n",
                shlex_quote_path(&marker)
            ),
        );

        invoke_install_script(
            &curl,
            &shell,
            "https://example.invalid/install.sh",
            "v0.98.1",
            Some("governed-test-token"),
            &install_dir,
            false,
        )
        .expect("installer invocation");

        let captured = std::fs::read_to_string(marker).expect("captured environment");
        assert_eq!(
            captured,
            format!("{}\n{}\n", install_dir.display(), curl.display())
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_installed_version_is_required_before_daemon_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        write_executable(&binary, "#!/bin/sh\nprintf 'shipyard 0.99.1\\n'\n");

        let mut exact = Command::new("/bin/sh");
        exact.arg(&binary);
        verify_installed_version_with_command(&mut exact, &binary, "v0.99.1")
            .expect("exact version");

        let mut mismatch = Command::new("/bin/sh");
        mismatch.arg(&binary);
        let error = verify_installed_version_with_command(&mut mismatch, &binary, "v0.99.2")
            .expect_err("mismatch");
        assert!(error.message.contains("daemon was not refreshed"));
    }

    #[cfg(unix)]
    #[test]
    fn daemon_refresh_executes_verified_binary_with_exact_runtime_context() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        let args_capture = temp.path().join("args");
        write_executable(
            &binary,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '%s\\n' '{{\"schema_version\":1,\"command\":\"daemon:refresh\",\"new_pid\":4242}}'\n",
                shlex_quote_path(&args_capture)
            ),
        );
        let global_dir = temp.path().join("governed config");
        let state_dir = temp.path().join("governed state");
        let runtime_paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(global_dir.clone()),
            Some(state_dir.clone()),
        );

        assert_eq!(
            refresh_daemon_with_installed_binary(RuntimeMode::Isolated, &runtime_paths, &binary,)
                .expect("refresh receipt"),
            4242
        );
        let captured = std::fs::read_to_string(args_capture).expect("captured argv");
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "--mode",
                "isolated",
                "--global-dir",
                global_dir.to_str().expect("global dir"),
                "--state-dir",
                state_dir.to_str().expect("state dir"),
                "--json",
                "daemon",
                "refresh",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn daemon_refresh_fails_closed_without_exact_typed_receipt() {
        let _process_fixture = crate::test_support::lock_process_tree_for_test();
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        write_executable(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"command\":\"daemon:start\",\"new_pid\":4242}'\n",
        );
        let runtime_paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("config")),
            Some(temp.path().join("state")),
        );

        let error =
            refresh_daemon_with_installed_binary(RuntimeMode::Isolated, &runtime_paths, &binary)
                .expect_err("wrong receipt type");
        assert!(error.contains("unexpected daemon refresh receipt"));
    }

    #[test]
    fn update_tool_overrides_must_be_absolute() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config = LoadedConfig::load_machine_global_from_dir(temp.path().to_path_buf())
            .expect("empty config");

        let cli_error = resolve_update_tool(Some(Path::new("curl")), &config, "curl_bin", "curl")
            .expect_err("relative CLI override");
        assert!(
            cli_error
                .message
                .contains("--curl-bin must be an absolute path")
        );

        std::fs::write(
            temp.path().join("config.toml"),
            "[update]\nshell_bin = \"bash\"\n",
        )
        .expect("config");
        let config = LoadedConfig::load_machine_global_from_dir(temp.path().to_path_buf())
            .expect("configured update tool");
        let config_error = resolve_update_tool(None, &config, "shell_bin", "bash")
            .expect_err("relative configured override");
        assert!(
            config_error
                .message
                .contains("update.shell_bin must be an absolute path")
        );
    }

    #[test]
    fn explicit_check_does_not_resolve_installer_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let runtime_paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("config")),
            Some(temp.path().join("state")),
        );
        let args = UpdateArgs {
            check: true,
            to: Some(format!("v{}", installed_version())),
            dry_run: false,
            refresh_daemon: false,
            unattended_fleet: false,
            install_script_url: None,
            releases_api_base: None,
            curl_bin: Some(PathBuf::from("missing-relative-curl")),
            shell_bin: Some(PathBuf::from("missing-relative-bash")),
        };
        let mut output = Vec::new();

        assert_eq!(
            update_command(
                &args,
                RuntimeMode::Isolated,
                temp.path(),
                &runtime_paths,
                false,
                &mut output,
            )
            .expect("read-only check"),
            ExitCode::SUCCESS
        );
    }

    #[test]
    fn implicit_non_newer_release_is_a_true_noop() {
        let args = UpdateArgs {
            check: false,
            to: None,
            dry_run: false,
            refresh_daemon: false,
            unattended_fleet: false,
            install_script_url: None,
            releases_api_base: None,
            curl_bin: None,
            shell_bin: None,
        };
        let missing = Path::new("/does/not/exist");
        let tools = UpdateTools {
            token: None,
            curl_bin: missing,
            shell_bin: missing,
            install_dir: missing,
        };

        assert!(
            !apply_update(
                &args,
                "0.100.0",
                "v0.99.0",
                false,
                &tools,
                false,
                &mut Vec::new(),
            )
            .expect("no-op")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unattended_check_proves_the_configured_command_helper_before_refresh() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_dir = temp.path().join("config");
        std::fs::create_dir(&config_dir).expect("config dir");
        let marker = temp.path().join("auth-ran");
        let helper = temp.path().join("token-helper");
        write_executable(
            &helper,
            &format!(
                "#!/bin/sh\n/usr/bin/touch {}\nprintf governed-test-token\n",
                shlex_quote_path(&marker)
            ),
        );
        std::fs::write(
            config_dir.join("config.toml"),
            format!(
                "[github.auth]\nsource = \"command\"\ntoken_command = [{}]\n",
                toml::Value::String(helper.display().to_string())
            ),
        )
        .expect("config");
        let runtime_paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(config_dir),
            Some(temp.path().join("state")),
        );
        let args = UpdateArgs {
            check: true,
            to: Some(format!("v{}", installed_version())),
            dry_run: false,
            refresh_daemon: false,
            unattended_fleet: true,
            install_script_url: None,
            releases_api_base: None,
            curl_bin: Some(PathBuf::from("missing-relative-curl")),
            shell_bin: Some(PathBuf::from("missing-relative-bash")),
        };

        update_command(
            &args,
            RuntimeMode::Isolated,
            temp.path(),
            &runtime_paths,
            false,
            &mut Vec::new(),
        )
        .expect("unattended auth proof");
        assert!(marker.exists());
    }

    #[test]
    fn unattended_fleet_requires_self_contained_command_auth() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("config.toml"),
            "[github.auth]\nsource = \"env\"\ntoken_env = \"SHIPYARD_TOKEN\"\n",
        )
        .expect("env config");
        let env_config = LoadedConfig::load_machine_global_from_dir(temp.path().to_path_buf())
            .expect("load env config");
        let error = validate_unattended_auth(&env_config).expect_err("env auth is stripped");
        assert!(error.message.contains("source = \"command\""));

        std::fs::write(
            temp.path().join("config.toml"),
            "[github.auth]\nsource = \"command\"\ntoken_command = [\"/usr/bin/false\"]\n",
        )
        .expect("command config");
        let command_config = LoadedConfig::load_machine_global_from_dir(temp.path().to_path_buf())
            .expect("load command config");
        validate_unattended_auth(&command_config).expect("command auth is self-contained");
    }

    #[cfg(unix)]
    fn shlex_quote_path(path: &Path) -> String {
        crate::executor::ssh::shlex_quote(&path.display().to_string())
    }

    #[test]
    fn normalize_tag_prepends_v() {
        assert_eq!(normalize_tag("0.53.0").expect("tag"), "v0.53.0");
        assert_eq!(normalize_tag("v0.53.0").expect("tag"), "v0.53.0");
        assert_eq!(normalize_tag("  v1.2.3 ").expect("tag"), "v1.2.3");
        assert!(normalize_tag("main").is_err());
        assert!(normalize_tag("v1.2.3-rc1").is_err());
    }

    #[test]
    fn parse_tag_name_extracts_tag_name() {
        let body = r#"{"tag_name":"v0.54.0","draft":false}"#;
        assert_eq!(parse_tag_name(body).unwrap(), "v0.54.0");
    }

    #[test]
    fn select_env_token_prefers_shipyard_then_gh_then_github() {
        let pick = |present: &[(&str, &str)]| {
            let owned: Vec<(String, String)> = present
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            select_env_token(|name| {
                owned
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
        };
        assert_eq!(
            pick(&[
                ("SHIPYARD_GITHUB_TOKEN", "sy"),
                ("GH_TOKEN", "gh"),
                ("GITHUB_TOKEN", "ght")
            ]),
            Some("sy".to_owned())
        );
        assert_eq!(
            pick(&[("GH_TOKEN", "gh"), ("GITHUB_TOKEN", "ght")]),
            Some("gh".to_owned())
        );
        assert_eq!(pick(&[("GITHUB_TOKEN", "ght")]), Some("ght".to_owned()));
        // Blank/whitespace values are ignored; nothing set -> None.
        assert_eq!(pick(&[("SHIPYARD_GITHUB_TOKEN", "   ")]), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn classify_release_response_detects_rate_limit() {
        let body =
            r#"{"message":"API rate limit exceeded for 1.2.3.4.","documentation_url":"..."}"#;
        let err = classify_release_response("403", body, false).expect_err("rate limited");
        assert!(err.contains("rate limit"));
        assert!(err.contains("GITHUB_TOKEN"));
        assert!(err.contains("60/hr"));
        // Authenticated 403 rate-limit gets the other message.
        let err = classify_release_response("403", body, true).expect_err("rate limited");
        assert!(err.contains("authenticated token"));
    }

    #[test]
    fn classify_release_response_passes_success_and_flags_other_errors() {
        let ok = r#"{"tag_name":"v0.68.0"}"#;
        assert_eq!(
            classify_release_response("200", ok, true).unwrap(),
            "v0.68.0"
        );
        // A non-2xx that is not a rate limit surfaces the status, not "no binary".
        let err =
            classify_release_response("404", r#"{"message":"Not Found"}"#, false).expect_err("404");
        assert!(err.contains("HTTP 404"));
        assert!(err.contains("Not Found"));
    }

    #[test]
    fn split_trailing_http_code_splits_last_newline_only() {
        // Pretty-printed JSON body (internal newlines) + appended status.
        let raw = "{\n  \"tag_name\": \"v0.68.0\"\n}\n200";
        let (body, code) = split_trailing_http_code(raw);
        assert_eq!(code, "200");
        assert_eq!(parse_tag_name(body).unwrap(), "v0.68.0");
    }

    #[test]
    fn parse_tag_name_errors_when_missing() {
        let body = r#"{"draft":false}"#;
        let err = parse_tag_name(body).expect_err("missing tag_name");
        assert!(err.contains("tag_name"));
    }

    #[test]
    fn target_is_newer_compares_semver_correctly() {
        assert!(target_is_newer("0.53.0", "v0.54.0"));
        assert!(!target_is_newer("0.54.0", "v0.54.0"));
        assert!(!target_is_newer("0.54.0", "v0.53.0"));
        assert!(target_is_newer("0.53.0", "v0.53.1"));
        assert!(target_is_newer("0.53.0", "v1.0.0"));
    }

    #[test]
    fn compare_semver_handles_prerelease_suffix() {
        // Pre-release suffixes are ignored conservatively.
        assert_eq!(
            compare_semver("0.54.0-rc.1", "0.54.0"),
            std::cmp::Ordering::Equal
        );
    }

    #[test]
    fn render_check_human_mentions_action() {
        let mut buf = Vec::new();
        render_check("0.53.0", "v0.54.0", true, false, false, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("update available"));
        assert!(text.contains("v0.54.0"));
        assert!(text.contains("0.53.0"));
    }

    #[test]
    fn render_check_human_handles_already_up_to_date() {
        let mut buf = Vec::new();
        render_check("0.54.0", "v0.54.0", false, false, false, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("already up to date"));
    }

    #[test]
    fn render_plan_human_describes_dry_run() {
        let mut buf = Vec::new();
        render_plan("0.53.0", "v0.54.0", true, false, &mut buf).expect("render");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains("Dry-run"));
        assert!(text.contains("install.sh"));
    }

    #[test]
    fn render_check_json_carries_full_envelope() {
        let mut buf = Vec::new();
        render_check("0.53.0", "v0.54.0", true, false, true, &mut buf).expect("render");
        let json: Value = serde_json::from_slice(&buf).expect("json");
        assert_eq!(json["installed"], Value::from("0.53.0"));
        assert_eq!(json["target"], Value::from("v0.54.0"));
        assert_eq!(json["update_available"], Value::Bool(true));
        assert_eq!(json["event"], Value::from("check"));
        assert_eq!(json["command"], Value::from(UPDATE_EVENT));
    }

    #[test]
    fn ordinary_update_does_not_overwrite_a_shadowing_source_binary() {
        let temp = tempfile::tempdir().expect("temp");
        let source_binary = temp.path().join("checkout/target/release/shipyard");
        let custom_install = temp.path().join("custom-install");
        assert_eq!(
            update_install_dir(false, &source_binary, None).expect("canonical install"),
            home_dir().join(".local/bin")
        );
        assert_eq!(
            update_install_dir(true, &source_binary, None).expect("governed install"),
            source_binary.parent().expect("source parent")
        );
        assert_eq!(
            update_install_dir(false, &source_binary, Some(&custom_install))
                .expect("custom install"),
            custom_install
        );
        assert!(
            update_install_dir(false, &source_binary, Some(Path::new("relative/bin"))).is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn update_clients_never_resolve_through_ambient_path() {
        assert_eq!(
            canonical_tool_candidates("curl"),
            vec![PathBuf::from("/usr/bin/curl"), PathBuf::from("/bin/curl")]
        );
        assert_eq!(
            canonical_tool_candidates("bash"),
            vec![PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")]
        );
        assert!(canonical_tool_candidates("curl").iter().all(|path| {
            !path.starts_with("/opt/homebrew") && !path.starts_with(home_dir().join(".local"))
        }));
    }
}
