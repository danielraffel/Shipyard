//! Shared GitHub CLI command boundary and auth resolution.

use std::env;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{ConfigLoadError, LoadedConfig};
use crate::identity::RuntimeMode;

const DEFAULT_REFRESH_SKEW_SECONDS: u64 = 60;
const GH_TOKEN_ENV: &str = "GH_TOKEN";
const GITHUB_TOKEN_ENV: &str = "GITHUB_TOKEN";
const GH_ENTERPRISE_TOKEN_ENV: &str = "GH_ENTERPRISE_TOKEN";
const GITHUB_ENTERPRISE_TOKEN_ENV: &str = "GITHUB_ENTERPRISE_TOKEN";

/// Auth-aware GitHub CLI command factory.
#[derive(Clone)]
pub struct GhClient {
    auth: GhAuthConfig,
    cache: Arc<Mutex<Option<CachedToken>>>,
}

impl Debug for GhClient {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GhClient")
            .field("auth", &self.auth)
            .finish_non_exhaustive()
    }
}

impl GhClient {
    /// Load Shipyard config from `cwd` and build a GitHub command client.
    pub fn from_cwd(mode: RuntimeMode, cwd: &Path) -> Result<Self, GhConfigError> {
        let config = LoadedConfig::load_from_cwd(mode, cwd).map_err(GhConfigError::Load)?;
        Self::from_loaded_config(&config)
    }

    /// Build a GitHub command client from an already loaded config.
    pub fn from_loaded_config(config: &LoadedConfig) -> Result<Self, GhConfigError> {
        let auth = GhAuthConfig::from_loaded_config(config)?;
        Ok(Self::new(auth))
    }

    /// Build a client that uses ambient `gh` auth.
    #[must_use]
    pub fn ambient() -> Self {
        Self::new(GhAuthConfig::ambient())
    }

    /// Prepare a `gh` command with the requested supervision and auth policy.
    ///
    /// The returned command has its program, current directory, supervised
    /// marker, and optional child-process `GH_TOKEN` set. Callers still own
    /// arguments, stdio, timeouts, and output classification.
    pub fn prepare_command(
        &self,
        cwd: &Path,
        binary_override: Option<&Path>,
        supervision: GhSupervision,
        auth_policy: GhAuthPolicy,
    ) -> Result<Command, GhPrepareError> {
        let mut command = match supervision {
            GhSupervision::Supervised => crate::supervised::gh_supervised(binary_override),
            GhSupervision::Unsupervised => {
                binary_override.map_or_else(|| Command::new("gh"), Command::new)
            }
        };
        command.current_dir(cwd);
        if auth_policy == GhAuthPolicy::AmbientOnly {
            command.env_remove(GH_TOKEN_ENV);
            command.env_remove(GITHUB_TOKEN_ENV);
            command.env_remove(GH_ENTERPRISE_TOKEN_ENV);
            command.env_remove(GITHUB_ENTERPRISE_TOKEN_ENV);
        }
        if auth_policy == GhAuthPolicy::Default
            && let Some(token) = self.resolve_token(cwd)?
        {
            command.env(GH_TOKEN_ENV, token.token);
        }
        Ok(command)
    }

    /// Resolve the effective auth source without exposing token material.
    pub fn auth_summary(
        &self,
        cwd: &Path,
        auth_policy: GhAuthPolicy,
    ) -> Result<GhAuthSummary, GhPrepareError> {
        if auth_policy == GhAuthPolicy::AmbientOnly {
            return Ok(GhAuthSummary {
                source: GhAuthSourceSummary::GhCli,
                token_kind: None,
                expires_at: None,
            });
        }

        match &self.auth.source {
            GhAuthSource::GhCli => Ok(GhAuthSummary {
                source: GhAuthSourceSummary::GhCli,
                token_kind: None,
                expires_at: None,
            }),
            GhAuthSource::Env { token_env } => {
                let token = env_token(token_env)?;
                Ok(GhAuthSummary {
                    source: GhAuthSourceSummary::Env {
                        token_env: token_env.clone(),
                    },
                    token_kind: token.kind,
                    expires_at: token.expires_at,
                })
            }
            GhAuthSource::Command { .. } => {
                let token = self
                    .resolve_token(cwd)?
                    .expect("command auth should resolve a token");
                Ok(GhAuthSummary {
                    source: GhAuthSourceSummary::Command,
                    token_kind: token.kind,
                    expires_at: token.expires_at,
                })
            }
        }
    }

    fn new(auth: GhAuthConfig) -> Self {
        Self {
            auth,
            cache: Arc::new(Mutex::new(None)),
        }
    }

    fn resolve_token(&self, cwd: &Path) -> Result<Option<TokenResolution>, GhPrepareError> {
        match &self.auth.source {
            GhAuthSource::GhCli => Ok(None),
            GhAuthSource::Env { token_env } => env_token(token_env).map(Some),
            GhAuthSource::Command {
                token_command,
                cache_ttl_seconds,
            } => self
                .resolve_command_token(cwd, token_command, *cache_ttl_seconds)
                .map(Some),
        }
    }

    fn resolve_command_token(
        &self,
        cwd: &Path,
        token_command: &[String],
        cache_ttl_seconds: Option<u64>,
    ) -> Result<TokenResolution, GhPrepareError> {
        let expanded = expand_token_command(token_command, cwd)?;
        let now = Utc::now();
        if let Some(cached) = self.cached_token(&expanded, now)? {
            return Ok(cached);
        }

        let (program, args) = expanded
            .split_first()
            .ok_or(GhPrepareError::EmptyTokenCommand)?;
        let output = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .output()
            .map_err(|source| GhPrepareError::HelperStart {
                program: program.clone(),
                source,
            })?;
        if !output.status.success() {
            return Err(GhPrepareError::HelperFailed {
                program: program.clone(),
                status: output.status.code(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let token = parse_helper_stdout(
            stdout.trim(),
            now,
            cache_ttl_seconds,
            self.auth.refresh_skew_seconds,
        )?;
        self.store_cached_token(expanded, &token, now)?;
        Ok(token)
    }

    fn cached_token(
        &self,
        key: &[String],
        now: DateTime<Utc>,
    ) -> Result<Option<TokenResolution>, GhPrepareError> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| GhPrepareError::TokenCachePoisoned)?;
        Ok(cache.as_ref().and_then(|cached| {
            (cached.key == key && cached.is_valid(now)).then(|| cached.token.clone())
        }))
    }

    fn store_cached_token(
        &self,
        key: Vec<String>,
        token: &TokenResolution,
        now: DateTime<Utc>,
    ) -> Result<(), GhPrepareError> {
        let Some(valid_until) = token.valid_until else {
            return Ok(());
        };
        if valid_until <= now {
            return Err(GhPrepareError::TokenExpired);
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| GhPrepareError::TokenCachePoisoned)?;
        *cache = Some(CachedToken {
            key,
            token: token.clone(),
            valid_until,
        });
        Ok(())
    }
}

/// Which auth source to apply when preparing a `gh` command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhAuthPolicy {
    /// Use configured Shipyard auth when present.
    Default,
    /// Ignore configured Shipyard auth and use the stored `gh` login.
    ///
    /// GitHub token environment variables are masked for the child process so
    /// this path cannot accidentally reuse the configured integration token.
    AmbientOnly,
}

/// Whether the prepared `gh` command should carry Shipyard's supervised marker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhSupervision {
    /// Add `SHIPYARD_PR_RUNNING=1`.
    Supervised,
    /// Do not add Shipyard's supervised marker.
    Unsupervised,
}

/// Sanitized summary of the effective GitHub auth source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GhAuthSummary {
    /// Effective auth source.
    pub source: GhAuthSourceSummary,
    /// Optional helper-reported token kind.
    pub token_kind: Option<String>,
    /// Optional helper-reported expiry.
    pub expires_at: Option<DateTime<Utc>>,
}

/// Sanitized effective auth source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GhAuthSourceSummary {
    /// Ambient `gh` auth.
    GhCli,
    /// Token read from an environment variable.
    Env {
        /// Environment variable name, not the token value.
        token_env: String,
    },
    /// Token read from a command helper.
    Command,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GhAuthConfig {
    source: GhAuthSource,
    refresh_skew_seconds: u64,
}

impl GhAuthConfig {
    fn ambient() -> Self {
        Self {
            source: GhAuthSource::GhCli,
            refresh_skew_seconds: DEFAULT_REFRESH_SKEW_SECONDS,
        }
    }

    fn from_loaded_config(config: &LoadedConfig) -> Result<Self, GhConfigError> {
        let Some(value) = config.get("github.auth") else {
            return Ok(Self::ambient());
        };
        let raw = value
            .clone()
            .try_into::<RawGithubAuthConfig>()
            .map_err(|source| GhConfigError::Parse { source })?;
        raw.into_config()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum GhAuthSource {
    GhCli,
    Env {
        token_env: String,
    },
    Command {
        token_command: Vec<String>,
        cache_ttl_seconds: Option<u64>,
    },
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawGithubAuthConfig {
    source: Option<String>,
    token_env: Option<String>,
    token_command: Option<Vec<String>>,
    cache_ttl_seconds: Option<u64>,
    refresh_skew_seconds: Option<u64>,
}

impl RawGithubAuthConfig {
    fn into_config(self) -> Result<GhAuthConfig, GhConfigError> {
        let refresh_skew_seconds = self
            .refresh_skew_seconds
            .unwrap_or(DEFAULT_REFRESH_SKEW_SECONDS);
        validate_seconds("refresh_skew_seconds", refresh_skew_seconds, true)?;
        if let Some(ttl) = self.cache_ttl_seconds {
            validate_seconds("cache_ttl_seconds", ttl, false)?;
        }

        let has_credential_fields = self.token_env.is_some() || self.token_command.is_some();
        let source = match self.source.as_deref() {
            Some(source) => source,
            None if has_credential_fields => {
                return Err(GhConfigError::Invalid {
                    message: "`github.auth.source` is required when token settings are present"
                        .to_owned(),
                });
            }
            None => "gh-cli",
        };

        let source = match source {
            "gh-cli" => GhAuthSource::GhCli,
            "env" => {
                let token_env = required_nonempty(self.token_env, "github.auth.token_env")?;
                GhAuthSource::Env { token_env }
            }
            "command" => {
                let token_command =
                    required_nonempty_vec(self.token_command, "github.auth.token_command")?;
                GhAuthSource::Command {
                    token_command,
                    cache_ttl_seconds: self.cache_ttl_seconds,
                }
            }
            other => {
                return Err(GhConfigError::Invalid {
                    message: format!(
                        "unsupported github.auth.source {other:?}; expected gh-cli, env, or command"
                    ),
                });
            }
        };

        Ok(GhAuthConfig {
            source,
            refresh_skew_seconds,
        })
    }
}

/// Configuration error for `[github.auth]`.
#[derive(Debug)]
pub enum GhConfigError {
    /// Loading Shipyard config failed.
    Load(ConfigLoadError),
    /// TOML shape did not match the expected auth schema.
    Parse {
        /// Underlying TOML deserialization error.
        source: toml::de::Error,
    },
    /// Auth config was syntactically valid but unsupported.
    Invalid {
        /// Human-readable config error.
        message: String,
    },
}

impl Display for GhConfigError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Load(source) => write!(f, "{source}"),
            Self::Parse { source } => write!(f, "failed to parse github.auth config: {source}"),
            Self::Invalid { message } => write!(f, "invalid github.auth config: {message}"),
        }
    }
}

impl Error for GhConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Load(source) => Some(source),
            Self::Parse { source } => Some(source),
            Self::Invalid { .. } => None,
        }
    }
}

/// Error while preparing a GitHub CLI command.
#[derive(Debug)]
pub enum GhPrepareError {
    /// Configured token environment variable was not available.
    MissingTokenEnv {
        /// Environment variable name.
        name: String,
    },
    /// Configured token environment variable was set but empty.
    EmptyTokenEnv {
        /// Environment variable name.
        name: String,
    },
    /// Command helper argv was empty.
    EmptyTokenCommand,
    /// Command helper failed to start.
    HelperStart {
        /// Helper executable.
        program: String,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Command helper exited non-zero.
    HelperFailed {
        /// Helper executable.
        program: String,
        /// Process exit code when available.
        status: Option<i32>,
        /// Helper stderr, trimmed.
        stderr: String,
    },
    /// Command helper stdout was empty.
    HelperStdoutEmpty,
    /// Command helper stdout looked like JSON but was malformed.
    HelperStdoutMalformed,
    /// Helper returned an expired or too-near-expiry token.
    TokenExpired,
    /// Repo placeholder expansion needed a GitHub remote.
    RepoSlugRequired,
    /// Git remote probing failed.
    RepoProbeFailed {
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// Internal token cache lock was poisoned.
    TokenCachePoisoned,
}

impl Display for GhPrepareError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingTokenEnv { name } => {
                write!(f, "configured token env var {name} is not set")
            }
            Self::EmptyTokenEnv { name } => {
                write!(f, "configured token env var {name} is empty")
            }
            Self::EmptyTokenCommand => write!(f, "configured token_command is empty"),
            Self::HelperStart { program, source } => {
                write!(f, "failed to start token helper {program:?}: {source}")
            }
            Self::HelperFailed {
                program,
                status,
                stderr,
            } => {
                let status = status.map_or_else(|| "signal".to_owned(), |code| code.to_string());
                if stderr.is_empty() {
                    write!(f, "token helper {program:?} exited with status {status}")
                } else {
                    let stderr = redact_token_like_text(stderr);
                    write!(
                        f,
                        "token helper {program:?} exited with status {status}: {stderr}"
                    )
                }
            }
            Self::HelperStdoutEmpty => write!(f, "token helper stdout was empty"),
            Self::HelperStdoutMalformed => write!(f, "token helper stdout was malformed"),
            Self::TokenExpired => write!(f, "token helper returned an expired token"),
            Self::RepoSlugRequired => write!(
                f,
                "token_command placeholder requires remote.origin.url to be a GitHub remote"
            ),
            Self::RepoProbeFailed { source } => write!(f, "git remote probe failed: {source}"),
            Self::TokenCachePoisoned => write!(f, "token cache lock was poisoned"),
        }
    }
}

impl Error for GhPrepareError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::HelperStart { source, .. } | Self::RepoProbeFailed { source } => Some(source),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TokenResolution {
    token: String,
    kind: Option<String>,
    expires_at: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CachedToken {
    key: Vec<String>,
    token: TokenResolution,
    valid_until: DateTime<Utc>,
}

impl CachedToken {
    fn is_valid(&self, now: DateTime<Utc>) -> bool {
        self.valid_until > now
    }
}

fn env_token(token_env: &str) -> Result<TokenResolution, GhPrepareError> {
    let token = env::var(token_env).map_err(|_| GhPrepareError::MissingTokenEnv {
        name: token_env.to_owned(),
    })?;
    if token.trim().is_empty() {
        return Err(GhPrepareError::EmptyTokenEnv {
            name: token_env.to_owned(),
        });
    }
    Ok(TokenResolution {
        token,
        kind: None,
        expires_at: None,
        valid_until: None,
    })
}

fn parse_helper_stdout(
    stdout: &str,
    now: DateTime<Utc>,
    cache_ttl_seconds: Option<u64>,
    refresh_skew_seconds: u64,
) -> Result<TokenResolution, GhPrepareError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(GhPrepareError::HelperStdoutEmpty);
    }
    if trimmed.starts_with('{') {
        return parse_json_helper_stdout(trimmed, now, cache_ttl_seconds, refresh_skew_seconds);
    }
    if trimmed.starts_with('[') {
        return Err(GhPrepareError::HelperStdoutMalformed);
    }
    Ok(TokenResolution {
        token: trimmed.to_owned(),
        kind: None,
        expires_at: None,
        valid_until: cache_ttl_seconds.map(|ttl| now + chrono::Duration::seconds(ttl_seconds(ttl))),
    })
}

fn parse_json_helper_stdout(
    stdout: &str,
    now: DateTime<Utc>,
    cache_ttl_seconds: Option<u64>,
    refresh_skew_seconds: u64,
) -> Result<TokenResolution, GhPrepareError> {
    let value =
        serde_json::from_str::<Value>(stdout).map_err(|_| GhPrepareError::HelperStdoutMalformed)?;
    let token = value
        .get("token")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or(GhPrepareError::HelperStdoutMalformed)?
        .to_owned();
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .filter(|kind| !kind.trim().is_empty())
        .map(ToOwned::to_owned);
    let expires_at = match value.get("expires_at") {
        Some(Value::String(expires_at)) => Some(
            DateTime::parse_from_rfc3339(expires_at)
                .map_err(|_| GhPrepareError::HelperStdoutMalformed)?
                .with_timezone(&Utc),
        ),
        Some(Value::Null) | None => None,
        Some(_) => return Err(GhPrepareError::HelperStdoutMalformed),
    };
    let valid_until = expires_at.map_or_else(
        || cache_ttl_seconds.map(|ttl| now + chrono::Duration::seconds(ttl_seconds(ttl))),
        |expires_at| {
            Some(expires_at - chrono::Duration::seconds(ttl_seconds(refresh_skew_seconds)))
        },
    );
    let token = TokenResolution {
        token,
        kind,
        expires_at,
        valid_until,
    };
    if token
        .valid_until
        .is_some_and(|valid_until| valid_until <= now)
    {
        return Err(GhPrepareError::TokenExpired);
    }
    Ok(token)
}

fn expand_token_command(args: &[String], cwd: &Path) -> Result<Vec<String>, GhPrepareError> {
    if args.is_empty() {
        return Err(GhPrepareError::EmptyTokenCommand);
    }
    let mut repo: Option<RepoIdentity> = None;
    args.iter()
        .map(|arg| {
            let mut expanded = arg.replace("{cwd}", &cwd.display().to_string());
            if needs_repo_placeholder(&expanded) {
                let identity = if let Some(identity) = &repo {
                    identity
                } else {
                    repo = Some(resolve_repo_identity(cwd)?);
                    repo.as_ref().expect("repo identity should be set")
                };
                expanded = expanded
                    .replace("{repo_slug}", &identity.slug)
                    .replace("{repo_owner}", &identity.owner)
                    .replace("{repo_name}", &identity.name);
            }
            Ok(expanded)
        })
        .collect()
}

fn needs_repo_placeholder(value: &str) -> bool {
    value.contains("{repo_slug}") || value.contains("{repo_owner}") || value.contains("{repo_name}")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RepoIdentity {
    slug: String,
    owner: String,
    name: String,
}

fn resolve_repo_identity(cwd: &Path) -> Result<RepoIdentity, GhPrepareError> {
    let output = Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(cwd)
        .output()
        .map_err(|source| GhPrepareError::RepoProbeFailed { source })?;
    if !output.status.success() {
        return Err(GhPrepareError::RepoSlugRequired);
    }
    let remote = String::from_utf8_lossy(&output.stdout);
    let slug = parse_github_remote_slug(remote.trim()).ok_or(GhPrepareError::RepoSlugRequired)?;
    let (owner, name) = slug
        .split_once('/')
        .ok_or(GhPrepareError::RepoSlugRequired)?;
    Ok(RepoIdentity {
        slug: slug.clone(),
        owner: owner.to_owned(),
        name: name.to_owned(),
    })
}

/// Parse a GitHub owner/repo slug from common origin remote URL forms.
#[must_use]
pub fn parse_github_remote_slug(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/');
    let remote = remote.strip_suffix(".git").unwrap_or(remote);
    [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .into_iter()
    .find_map(|prefix| remote.strip_prefix(prefix))
    .and_then(|path| {
        let mut parts = path.split('/');
        let owner = parts.next()?;
        let repo = parts.next()?;
        if owner.is_empty() || repo.is_empty() || parts.next().is_some() {
            return None;
        }
        Some(format!("{owner}/{repo}"))
    })
}

/// Detect a surface GraphQL API rate-limit message from `gh`.
#[must_use]
pub fn is_graphql_rate_limited(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("graphql") && lower.contains("rate limit")
}

fn required_nonempty(value: Option<String>, key: &str) -> Result<String, GhConfigError> {
    let value = value.ok_or_else(|| GhConfigError::Invalid {
        message: format!("`{key}` is required"),
    })?;
    if value.trim().is_empty() {
        return Err(GhConfigError::Invalid {
            message: format!("`{key}` must not be empty"),
        });
    }
    Ok(value)
}

fn required_nonempty_vec(
    value: Option<Vec<String>>,
    key: &str,
) -> Result<Vec<String>, GhConfigError> {
    let value = value.ok_or_else(|| GhConfigError::Invalid {
        message: format!("`{key}` is required"),
    })?;
    if value.is_empty() || value.iter().any(|item| item.trim().is_empty()) {
        return Err(GhConfigError::Invalid {
            message: format!("`{key}` must contain non-empty argv entries"),
        });
    }
    Ok(value)
}

fn validate_seconds(name: &str, value: u64, allow_zero: bool) -> Result<(), GhConfigError> {
    if !allow_zero && value == 0 {
        return Err(GhConfigError::Invalid {
            message: format!("`github.auth.{name}` must be greater than zero"),
        });
    }
    if value > i64::MAX as u64 {
        return Err(GhConfigError::Invalid {
            message: format!("`github.auth.{name}` is too large"),
        });
    }
    Ok(())
}

fn ttl_seconds(value: u64) -> i64 {
    i64::try_from(value).expect("seconds value should be validated before use")
}

fn redact_token_like_text(input: &str) -> String {
    const TOKEN_PREFIXES: &[&str] = &["github_pat_", "ghp_", "gho_", "ghu_", "ghs_", "ghr_"];

    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        let rest = &input[index..];
        if let Some(prefix) = TOKEN_PREFIXES
            .iter()
            .find(|prefix| rest.starts_with(**prefix))
        {
            let token_len = rest
                .char_indices()
                .find_map(|(offset, ch)| {
                    (!is_token_char(ch) && offset >= prefix.len()).then_some(offset)
                })
                .unwrap_or(rest.len());
            output.push_str("<redacted-token>");
            index += token_len;
            continue;
        }
        let ch = rest.chars().next().expect("non-empty rest");
        output.push(ch);
        index += ch.len_utf8();
    }
    output
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    use tempfile::TempDir;

    use super::*;
    use crate::config::LocalOverlaySource;

    fn config_from_toml(input: &str) -> LoadedConfig {
        LoadedConfig {
            data: input.parse::<toml::Table>().expect("parse toml"),
            global_dir: PathBuf::from("/tmp/shipyard-global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn env_value(command: &Command, key: &str) -> Option<OsString> {
        command.get_envs().find_map(|(name, value)| {
            (name == key).then(|| value.map(std::ffi::OsStr::to_owned))?
        })
    }

    fn env_removed(command: &Command, key: &str) -> bool {
        command
            .get_envs()
            .any(|(name, value)| name == key && value.is_none())
    }

    #[test]
    fn missing_config_uses_ambient_auth() {
        let config = config_from_toml("");
        let client = GhClient::from_loaded_config(&config).expect("client");
        assert_eq!(client.auth.source, GhAuthSource::GhCli);
    }

    #[test]
    fn parses_env_auth_config() {
        let config = config_from_toml(
            r#"
            [github.auth]
            source = "env"
            token_env = "SHIPYARD_GITHUB_TOKEN"
            "#,
        );
        let client = GhClient::from_loaded_config(&config).expect("client");
        assert_eq!(
            client.auth.source,
            GhAuthSource::Env {
                token_env: "SHIPYARD_GITHUB_TOKEN".to_owned()
            }
        );
    }

    #[test]
    fn rejects_token_settings_without_source() {
        let config = config_from_toml(
            r#"
            [github.auth]
            token_env = "SHIPYARD_GITHUB_TOKEN"
            "#,
        );
        let error = GhClient::from_loaded_config(&config).expect_err("invalid config");
        assert!(error.to_string().contains("source"));
    }

    #[test]
    fn rejects_empty_command_auth_config() {
        let config = config_from_toml(
            r#"
            [github.auth]
            source = "command"
            token_command = []
            "#,
        );
        let error = GhClient::from_loaded_config(&config).expect_err("invalid config");
        assert!(error.to_string().contains("token_command"));
    }

    #[test]
    fn prepare_command_injects_env_token_and_supervised_marker() {
        let config = config_from_toml(
            r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            "#,
        );
        let expected_path = env::var("PATH").expect("PATH");
        let client = GhClient::from_loaded_config(&config).expect("client");
        let command = client
            .prepare_command(
                Path::new("/tmp"),
                Some(Path::new("/tmp/fake-gh")),
                GhSupervision::Supervised,
                GhAuthPolicy::Default,
            )
            .expect("command");

        assert_eq!(command.get_program(), Path::new("/tmp/fake-gh").as_os_str());
        assert_eq!(
            env_value(&command, GH_TOKEN_ENV).as_deref(),
            Some(OsString::from(expected_path).as_os_str())
        );
        assert_eq!(
            env_value(&command, crate::supervised::SUPERVISED_ENV_VAR).as_deref(),
            Some(OsString::from(crate::supervised::SUPERVISED_ENV_VALUE).as_os_str())
        );
    }

    #[test]
    fn ambient_only_does_not_inject_configured_token() {
        let config = config_from_toml(
            r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            "#,
        );
        let client = GhClient::from_loaded_config(&config).expect("client");
        let command = client
            .prepare_command(
                Path::new("/tmp"),
                None,
                GhSupervision::Unsupervised,
                GhAuthPolicy::AmbientOnly,
            )
            .expect("command");
        assert_eq!(env_value(&command, GH_TOKEN_ENV), None);
    }

    #[test]
    fn ambient_only_masks_inherited_github_token_env_vars() {
        let config = config_from_toml("");
        let client = GhClient::from_loaded_config(&config).expect("client");
        let command = client
            .prepare_command(
                Path::new("/tmp"),
                None,
                GhSupervision::Unsupervised,
                GhAuthPolicy::AmbientOnly,
            )
            .expect("command");

        assert!(env_removed(&command, GH_TOKEN_ENV));
        assert!(env_removed(&command, GITHUB_TOKEN_ENV));
        assert!(env_removed(&command, GH_ENTERPRISE_TOKEN_ENV));
        assert!(env_removed(&command, GITHUB_ENTERPRISE_TOKEN_ENV));
    }

    #[test]
    fn parses_plain_helper_stdout_with_ttl() {
        let now = Utc::now();
        let token =
            parse_helper_stdout("ghp_plain\n", now, Some(300), DEFAULT_REFRESH_SKEW_SECONDS)
                .expect("token");
        assert_eq!(token.token, "ghp_plain");
        assert!(
            token
                .valid_until
                .is_some_and(|valid_until| valid_until > now)
        );
    }

    #[test]
    fn parses_json_helper_stdout_with_expiry() {
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(600);
        let stdout = serde_json::json!({
            "token": "ghs_json",
            "expires_at": expires_at.to_rfc3339(),
            "kind": "github-app-installation"
        })
        .to_string();
        let token =
            parse_helper_stdout(&stdout, now, None, DEFAULT_REFRESH_SKEW_SECONDS).expect("token");
        assert_eq!(token.token, "ghs_json");
        assert_eq!(token.kind.as_deref(), Some("github-app-installation"));
        assert_eq!(token.expires_at, Some(expires_at));
        assert!(
            token
                .valid_until
                .is_some_and(|valid_until| valid_until < expires_at)
        );
    }

    #[test]
    fn rejects_expired_json_helper_token() {
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(30);
        let stdout = serde_json::json!({
            "token": "ghs_json",
            "expires_at": expires_at.to_rfc3339()
        })
        .to_string();
        let error = parse_helper_stdout(&stdout, now, None, DEFAULT_REFRESH_SKEW_SECONDS)
            .expect_err("expired");
        assert!(matches!(error, GhPrepareError::TokenExpired));
    }

    #[test]
    fn expands_repo_placeholders() {
        let repo = TempDir::new().expect("tempdir");
        git(repo.path(), &["init", "--quiet", "--initial-branch=main"]);
        git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let command = vec![
            "helper".to_owned(),
            "--repo".to_owned(),
            "{repo_slug}".to_owned(),
            "--owner".to_owned(),
            "{repo_owner}".to_owned(),
            "--name".to_owned(),
            "{repo_name}".to_owned(),
            "--cwd".to_owned(),
            "{cwd}".to_owned(),
        ];
        let expanded = expand_token_command(&command, repo.path()).expect("expanded");
        assert_eq!(expanded[2], "owner/repo");
        assert_eq!(expanded[4], "owner");
        assert_eq!(expanded[6], "repo");
        assert_eq!(expanded[8], repo.path().display().to_string());
    }

    #[test]
    fn parses_github_remote_urls() {
        assert_eq!(
            parse_github_remote_slug("git@github.com:owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_remote_slug("ssh://git@github.com/owner/repo.git").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_remote_slug("https://github.com/owner/repo").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            parse_github_remote_slug("https://example.com/owner/repo"),
            None
        );
    }

    #[test]
    fn detects_graphql_rate_limit_text() {
        assert!(is_graphql_rate_limited("GraphQL: API rate limit exceeded"));
        assert!(!is_graphql_rate_limited("REST API rate limit exceeded"));
    }

    #[test]
    fn helper_failure_display_redacts_token_like_stderr() {
        let error = GhPrepareError::HelperFailed {
            program: "helper".to_owned(),
            status: Some(1),
            stderr: "failed after minting ghs_secret123 and github_pat_abcDEF".to_owned(),
        };

        let rendered = error.to_string();

        assert!(rendered.contains("<redacted-token>"));
        assert!(!rendered.contains("ghs_secret123"));
        assert!(!rendered.contains("github_pat_abcDEF"));
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(status.success(), "git command failed: {args:?}");
    }
}
