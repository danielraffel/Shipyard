//! Shared GitHub CLI command boundary and auth resolution.

use std::env;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::config::{ConfigLoadError, LoadedConfig};
use crate::identity::RuntimeMode;

const DEFAULT_REFRESH_SKEW_SECONDS: u64 = 60;
const GH_TOKEN_ENV: &str = "GH_TOKEN";
const PR_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Auth-aware GitHub CLI command factory.
#[derive(Clone)]
pub struct GhClient {
    auth: GhAuthConfig,
    cache: Arc<Mutex<Option<CachedToken>>>,
    /// Repo to use for a `{repo_slug}` token-command placeholder when the
    /// working directory has no GitHub remote. The daemon serves explicit
    /// `--repo` values but runs from a non-repo CWD, so without this a
    /// `token_command` using `{repo_slug}` could never mint a token.
    repo_hint: Option<RepoIdentity>,
    /// Explicit command target; unlike a hint, this outranks the checkout.
    repo_override: Option<RepoIdentity>,
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

    /// Set the repo a `{repo_slug}` token-command placeholder should expand to
    /// when the working directory isn't a GitHub checkout (the daemon case).
    /// A non-GitHub slug is ignored (the CWD path still applies).
    #[must_use]
    pub fn with_repo_hint(mut self, slug: &str) -> Self {
        self.repo_hint = RepoIdentity::from_slug(slug);
        self
    }

    /// Force token-command repository placeholders to use an explicit target.
    pub fn with_repo_override(mut self, slug: &str) -> Result<Self, GhPrepareError> {
        self.repo_override =
            Some(
                RepoIdentity::from_slug(slug).ok_or_else(|| GhPrepareError::InvalidRepoSlug {
                    slug: slug.to_owned(),
                })?,
            );
        Ok(self)
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
        self.prepare_command_inner(cwd, binary_override, supervision, auth_policy, None)
    }

    /// Prepare a `gh` command while bounding configured token-helper execution.
    pub fn prepare_command_with_auth_timeout(
        &self,
        cwd: &Path,
        binary_override: Option<&Path>,
        supervision: GhSupervision,
        auth_policy: GhAuthPolicy,
        auth_timeout: Duration,
    ) -> Result<Command, GhPrepareError> {
        self.prepare_command_inner(
            cwd,
            binary_override,
            supervision,
            auth_policy,
            Some(auth_timeout),
        )
    }

    fn prepare_command_inner(
        &self,
        cwd: &Path,
        binary_override: Option<&Path>,
        supervision: GhSupervision,
        auth_policy: GhAuthPolicy,
        auth_timeout: Option<Duration>,
    ) -> Result<Command, GhPrepareError> {
        let mut command = match supervision {
            GhSupervision::Supervised => crate::supervised::gh_supervised(binary_override),
            GhSupervision::Unsupervised => {
                binary_override.map_or_else(|| Command::new("gh"), Command::new)
            }
        };
        command.current_dir(cwd);
        if auth_policy == GhAuthPolicy::Default
            && let Some(token) = self.resolve_token_with_timeout(cwd, auth_timeout)?
        {
            command.env(GH_TOKEN_ENV, token.token);
        }
        Ok(command)
    }

    /// Prepare a non-interactive `git` command that uses the same configured
    /// GitHub identity as `prepare_command`.
    ///
    /// The credential helper receives `GH_TOKEN` through the child
    /// environment; token material is never placed in argv or a remote URL.
    pub fn prepare_git_command(&self, cwd: &Path) -> Result<Command, GhPrepareError> {
        let mut command = crate::supervised::git_supervised();
        command
            .current_dir(cwd)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", "!gh auth git-credential");
        if let Some(token) = self.resolve_token(cwd)? {
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
            repo_hint: None,
            repo_override: None,
        }
    }

    fn resolve_token(&self, cwd: &Path) -> Result<Option<TokenResolution>, GhPrepareError> {
        self.resolve_token_with_timeout(cwd, None)
    }

    fn resolve_token_with_timeout(
        &self,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<Option<TokenResolution>, GhPrepareError> {
        match &self.auth.source {
            GhAuthSource::GhCli => Ok(None),
            GhAuthSource::Env { token_env } => env_token(token_env).map(Some),
            GhAuthSource::Command {
                token_command,
                cache_ttl_seconds,
            } => self
                .resolve_command_token(cwd, token_command, *cache_ttl_seconds, timeout)
                .map(Some),
        }
    }

    fn resolve_command_token(
        &self,
        cwd: &Path,
        token_command: &[String],
        cache_ttl_seconds: Option<u64>,
        timeout: Option<Duration>,
    ) -> Result<TokenResolution, GhPrepareError> {
        let expanded = expand_token_command(
            token_command,
            cwd,
            self.repo_hint.as_ref(),
            self.repo_override.as_ref(),
        )?;
        let now = Utc::now();
        if let Some(cached) = self.cached_token(&expanded, now)? {
            return Ok(cached);
        }

        let (program, args) = expanded
            .split_first()
            .ok_or(GhPrepareError::EmptyTokenCommand)?;
        let mut command = Command::new(program);
        command.args(args).current_dir(cwd);
        let output = if let Some(timeout) = timeout {
            run_helper_with_timeout(&mut command, program, timeout)?
        } else {
            command
                .output()
                .map_err(|source| GhPrepareError::HelperStart {
                    program: program.clone(),
                    source,
                })?
        };
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

fn run_helper_with_timeout(
    command: &mut Command,
    program: &str,
    timeout: Duration,
) -> Result<Output, GhPrepareError> {
    let mut stdout = tempfile::tempfile().map_err(|source| GhPrepareError::HelperStart {
        program: program.to_owned(),
        source,
    })?;
    let mut stderr = tempfile::tempfile().map_err(|source| GhPrepareError::HelperStart {
        program: program.to_owned(),
        source,
    })?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout.try_clone().map_err(|source| {
            GhPrepareError::HelperStart {
                program: program.to_owned(),
                source,
            }
        })?))
        .stderr(Stdio::from(stderr.try_clone().map_err(|source| {
            GhPrepareError::HelperStart {
                program: program.to_owned(),
                source,
            }
        })?));
    let mut process_tree = crate::process::ProcessTree::spawn(command).map_err(|source| {
        GhPrepareError::HelperStart {
            program: program.to_owned(),
            source,
        }
    })?;
    let status = match process_tree.wait_timeout(timeout) {
        Ok(status) => status,
        Err(source) => {
            process_tree.terminate();
            return Err(GhPrepareError::HelperStart {
                program: program.to_owned(),
                source,
            });
        }
    };
    let Some(status) = status else {
        process_tree.terminate();
        return Err(GhPrepareError::HelperTimedOut {
            program: program.to_owned(),
            timeout_ms: timeout.as_millis(),
        });
    };
    // A successful helper is a bounded operation, not a daemon launcher.
    // Reap descendants that detached their stdio before the leader exited.
    process_tree.terminate();
    let read_output = |file: &mut std::fs::File| -> Result<Vec<u8>, GhPrepareError> {
        file.seek(SeekFrom::Start(0))
            .and_then(|_| {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).map(|_| bytes)
            })
            .map_err(|source| GhPrepareError::HelperStart {
                program: program.to_owned(),
                source,
            })
    };
    Ok(Output {
        status,
        stdout: read_output(&mut stdout)?,
        stderr: read_output(&mut stderr)?,
    })
}

/// Which auth source to apply when preparing a `gh` command.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GhAuthPolicy {
    /// Use configured Shipyard auth when present.
    Default,
    /// Ignore configured Shipyard auth and use ambient `gh` auth.
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
    /// Command helper exceeded the caller's bounded auth budget.
    HelperTimedOut {
        /// Helper executable.
        program: String,
        /// Timeout in milliseconds.
        timeout_ms: u128,
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
    /// An explicit repository override was not an exact `OWNER/REPO` slug.
    InvalidRepoSlug {
        /// Rejected repository slug.
        slug: String,
    },
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
            Self::HelperTimedOut {
                program,
                timeout_ms,
            } => write!(f, "token helper {program:?} timed out after {timeout_ms}ms"),
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
            Self::InvalidRepoSlug { slug } => write!(
                f,
                "invalid explicit GitHub repository slug {slug:?}; expected OWNER/REPO"
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

impl GhPrepareError {
    /// Whether retrying command preparation may recover without configuration
    /// or credential changes.
    #[must_use]
    pub(crate) fn is_transient(&self) -> bool {
        match self {
            Self::HelperTimedOut { .. } => true,
            Self::HelperStart { source, .. } => matches!(
                source.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::Interrupted
                    | std::io::ErrorKind::NotConnected
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::WouldBlock
            ),
            Self::HelperFailed { stderr, .. } => helper_failure_is_transient(stderr),
            Self::MissingTokenEnv { .. }
            | Self::EmptyTokenEnv { .. }
            | Self::EmptyTokenCommand
            | Self::HelperStdoutEmpty
            | Self::HelperStdoutMalformed
            | Self::TokenExpired
            | Self::RepoSlugRequired
            | Self::InvalidRepoSlug { .. }
            | Self::RepoProbeFailed { .. }
            | Self::TokenCachePoisoned => false,
        }
    }
}

fn helper_failure_is_transient(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "bad gateway",
        "connection aborted",
        "connection closed",
        "connection refused",
        "connection reset",
        "gateway timeout",
        "network is unreachable",
        "remote end closed connection",
        "service unavailable",
        "temporary failure",
        "temporarily unavailable",
        "timed out",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
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

fn expand_token_command(
    args: &[String],
    cwd: &Path,
    repo_hint: Option<&RepoIdentity>,
    repo_override: Option<&RepoIdentity>,
) -> Result<Vec<String>, GhPrepareError> {
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
                    repo = Some(resolve_repo_placeholder(cwd, repo_hint, repo_override)?);
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

/// Resolve the repo for a `{repo_slug}`-style placeholder: prefer the CWD's
/// GitHub remote (the interactive CLI case), but fall back to an explicit hint
/// when the CWD isn't a GitHub checkout (the daemon serves explicit `--repo`
/// values from a non-repo CWD).
fn resolve_repo_placeholder(
    cwd: &Path,
    repo_hint: Option<&RepoIdentity>,
    repo_override: Option<&RepoIdentity>,
) -> Result<RepoIdentity, GhPrepareError> {
    if let Some(repo_override) = repo_override {
        return Ok(repo_override.clone());
    }
    match resolve_repo_identity(cwd) {
        Ok(identity) => Ok(identity),
        Err(GhPrepareError::RepoSlugRequired) if repo_hint.is_some() => {
            Ok(repo_hint.expect("repo hint present").clone())
        }
        Err(error) => Err(error),
    }
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

impl RepoIdentity {
    /// Build from an `owner/name` slug. Returns `None` if it isn't a
    /// well-formed `owner/name` (so a bogus hint can't poison expansion).
    fn from_slug(slug: &str) -> Option<Self> {
        let slug = slug.trim();
        let (owner, name) = slug.split_once('/')?;
        if owner.is_empty() || name.is_empty() || name.contains('/') {
            return None;
        }
        Some(Self {
            slug: slug.to_owned(),
            owner: owner.to_owned(),
            name: name.to_owned(),
        })
    }
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

/// Return the merged head SHA of pull request `pr`, or `None` if it is not
/// merged (or cannot be observed).
///
/// Uses `gh pr view --json state,headRefOid` against the CWD's remote (or a
/// `snapshot_file` for testing). Returns `Some(head_sha)` only when the PR
/// state is exactly "merged" (case-insensitive), carrying the head commit the
/// merge landed on. Fails closed — returns `None` on any transport error,
/// missing token, or malformed response.
///
/// Returning the head SHA lets the queue-scheduler observation honour the
/// "never cancel when the merged head differs" guard: callers compare the
/// merged head against the queued job's expected head and only cancel on an
/// exact match.
#[must_use]
pub fn pr_merged_head_sha(
    client: Option<&GhClient>,
    repo: &str,
    pr: u64,
    cwd: &Path,
    snapshot_file: Option<&Path>,
) -> Option<String> {
    pr_merged_head_sha_with_options(
        client,
        repo,
        pr,
        cwd,
        snapshot_file,
        None,
        PR_OBSERVATION_TIMEOUT,
    )
}

fn pr_merged_head_sha_with_options(
    client: Option<&GhClient>,
    repo: &str,
    pr: u64,
    cwd: &Path,
    snapshot_file: Option<&Path>,
    binary_override: Option<&Path>,
    timeout: Duration,
) -> Option<String> {
    let value = if let Some(path) = snapshot_file {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    } else {
        let started = std::time::Instant::now();
        let client = client?.clone().with_repo_override(repo).ok()?;
        let mut cmd = client
            .prepare_command_with_auth_timeout(
                cwd,
                binary_override,
                GhSupervision::Supervised,
                GhAuthPolicy::Default,
                timeout,
            )
            .ok()?;
        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return None;
        }
        cmd.args([
            "pr",
            "view",
            &pr.to_string(),
            "--repo",
            repo,
            "--json",
            "state,headRefOid",
        ]);
        run_helper_with_timeout(&mut cmd, "gh", remaining)
            .ok()
            .filter(|out| out.status.success())
            .and_then(|out| serde_json::from_slice::<Value>(&out.stdout).ok())
    };
    let value = value?;
    let merged = value
        .get("state")
        .and_then(Value::as_str)
        .is_some_and(|state| state.eq_ignore_ascii_case("merged"));
    if !merged {
        return None;
    }
    value
        .get("headRefOid")
        .and_then(Value::as_str)
        .map(str::to_owned)
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

    #[cfg(unix)]
    fn write_executable(path: &Path, contents: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, contents).expect("write script");
        let mut permissions = std::fs::metadata(path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).expect("chmod script");
    }

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

    #[cfg(unix)]
    #[test]
    fn bounded_command_preparation_times_out_token_helper() {
        let temp = TempDir::new().expect("temp");
        let helper = temp.path().join("token-helper");
        write_executable(&helper, "#!/bin/sh\nsleep 5\nprintf token\n");
        let config = config_from_toml(&format!(
            r#"
            [github.auth]
            source = "command"
            token_command = ["{}"]
            "#,
            helper.display()
        ));
        let client = GhClient::from_loaded_config(&config).expect("client");
        let started = std::time::Instant::now();
        let error = client
            .prepare_command_with_auth_timeout(
                temp.path(),
                Some(Path::new("/tmp/fake-gh")),
                GhSupervision::Unsupervised,
                GhAuthPolicy::Default,
                Duration::from_millis(20),
            )
            .expect_err("helper timeout");
        assert!(matches!(error, GhPrepareError::HelperTimedOut { .. }));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn merged_pr_observation_uses_configured_auth_and_explicit_repository() {
        let temp = TempDir::new().expect("temp");
        let fake_gh = temp.path().join("gh");
        let invocation = temp.path().join("invocation");
        write_executable(
            &fake_gh,
            &format!(
                r#"#!/bin/sh
printf '%s\n%s' "$GH_TOKEN" "$*" > '{}'
printf '%s' '{{"state":"MERGED","headRefOid":"abc123"}}'
"#,
                invocation.display()
            ),
        );
        let config = config_from_toml(
            r#"
            [github.auth]
            source = "env"
            token_env = "PATH"
            "#,
        );
        let client = GhClient::from_loaded_config(&config).expect("client");

        let merged = pr_merged_head_sha_with_options(
            Some(&client),
            "owner/repo",
            42,
            temp.path(),
            None,
            Some(&fake_gh),
            Duration::from_secs(2),
        );

        assert_eq!(merged.as_deref(), Some("abc123"));
        let invocation = std::fs::read_to_string(invocation).expect("invocation");
        let (token, args) = invocation.split_once('\n').expect("token and args");
        assert!(!token.is_empty());
        assert_eq!(args, "pr view 42 --repo owner/repo --json state,headRefOid");
    }

    #[cfg(unix)]
    #[test]
    fn merged_pr_observation_kills_a_hung_github_command() {
        let temp = TempDir::new().expect("temp");
        let fake_gh = temp.path().join("gh");
        write_executable(&fake_gh, "#!/bin/sh\nsleep 5\n");
        let client = GhClient::ambient();
        let started = std::time::Instant::now();

        let merged = pr_merged_head_sha_with_options(
            Some(&client),
            "owner/repo",
            42,
            temp.path(),
            None,
            Some(&fake_gh),
            Duration::from_millis(20),
        );

        assert_eq!(merged, None);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_helper_receives_eof_instead_of_inheriting_caller_stdin() {
        let temp = TempDir::new().expect("temp");
        let helper = temp.path().join("token-helper");
        write_executable(&helper, "#!/bin/sh\nread ignored || true\nprintf token\n");
        let mut command = Command::new(&helper);
        // A caller-provided pipe would remain open in the parent and make the
        // helper block on `read` unless the bounded boundary replaces stdin.
        command.stdin(Stdio::piped());

        // Keep the assertion about EOF, not scheduler latency in the highly
        // parallel full suite. The helper returns immediately on the good path.
        let output = run_helper_with_timeout(&mut command, "token-helper", Duration::from_secs(30))
            .expect("helper should see EOF");

        assert!(output.status.success());
        assert_eq!(output.stdout, b"token");
    }

    #[cfg(unix)]
    #[test]
    fn successful_bounded_helper_does_not_leave_detached_descendants() {
        let temp = TempDir::new().expect("temp");
        let helper = temp.path().join("token-helper");
        let descendant_pid = temp.path().join("descendant.pid");
        write_executable(
            &helper,
            &format!(
                "#!/bin/sh\nsleep 120 </dev/null >/dev/null 2>&1 &\nprintf '%s\\n' \"$!\" > '{}'\nprintf token\n",
                descendant_pid.display()
            ),
        );
        let mut command = Command::new(&helper);

        // Full macOS CI runs many process-heavy tests concurrently. Keep this
        // boundary comfortably above scheduler latency while the descendant's
        // 120-second lifetime still proves cleanup rather than natural exit.
        let output = run_helper_with_timeout(&mut command, "token-helper", Duration::from_secs(30))
            .expect("helper succeeds");
        let pid = std::fs::read_to_string(&descendant_pid).expect("descendant pid");
        let pid = pid.trim();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let mut running = true;
        while running && std::time::Instant::now() < deadline {
            running = Command::new("kill")
                .args(["-0", pid])
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success());
            if running {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if running {
            let _ = Command::new("kill")
                .args(["-KILL", pid])
                .stderr(Stdio::null())
                .status();
        }

        assert!(output.status.success());
        assert_eq!(output.stdout, b"token");
        assert!(!running, "helper descendant {pid} survived success");
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
        // The CWD's GitHub remote wins even when a hint is present.
        let hint = RepoIdentity::from_slug("hintowner/hintrepo");
        let expanded =
            expand_token_command(&command, repo.path(), hint.as_ref(), None).expect("expanded");
        assert_eq!(expanded[2], "owner/repo");
        assert_eq!(expanded[4], "owner");
        assert_eq!(expanded[6], "repo");
        assert_eq!(expanded[8], repo.path().display().to_string());

        let repo_override = RepoIdentity::from_slug("target/other").expect("override");
        let overridden =
            expand_token_command(&command, repo.path(), hint.as_ref(), Some(&repo_override))
                .expect("overridden");
        assert_eq!(overridden[2], "target/other");
        assert_eq!(overridden[4], "target");
        assert_eq!(overridden[6], "other");
    }

    #[test]
    fn repo_placeholder_falls_back_to_hint_when_cwd_is_not_a_repo() {
        // The daemon case: CWD has no GitHub remote, so `{repo_slug}` must come
        // from the explicit hint (the served `--repo`) instead of erroring.
        let not_a_repo = TempDir::new().expect("tempdir");
        let command = vec![
            "helper".to_owned(),
            "--repo".to_owned(),
            "{repo_slug}".to_owned(),
        ];
        let hint = RepoIdentity::from_slug("danielraffel/pulp").expect("valid slug");
        let expanded =
            expand_token_command(&command, not_a_repo.path(), Some(&hint), None).expect("expanded");
        assert_eq!(expanded[2], "danielraffel/pulp");

        // Without a hint, it still errors (unchanged behavior).
        let err = expand_token_command(&command, not_a_repo.path(), None, None);
        assert!(matches!(err, Err(GhPrepareError::RepoSlugRequired)));
    }

    #[test]
    fn repo_identity_from_slug_rejects_malformed() {
        assert!(RepoIdentity::from_slug("owner/name").is_some());
        assert!(RepoIdentity::from_slug("nope").is_none());
        assert!(RepoIdentity::from_slug("owner/").is_none());
        assert!(RepoIdentity::from_slug("/name").is_none());
        assert!(RepoIdentity::from_slug("a/b/c").is_none());
    }

    #[test]
    fn explicit_repo_override_rejects_malformed_instead_of_falling_back() {
        let error = GhClient::ambient()
            .with_repo_override("owner/repo/extra")
            .expect_err("invalid explicit slug");

        assert!(matches!(
            error,
            GhPrepareError::InvalidRepoSlug { slug } if slug == "owner/repo/extra"
        ));
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
    fn authenticated_git_command_uses_environment_credential_helper() {
        let cwd = TempDir::new().expect("tempdir");
        let command = GhClient::ambient()
            .prepare_git_command(cwd.path())
            .expect("git command");
        assert_eq!(
            command.get_program().to_string_lossy(),
            crate::supervised::git_supervised()
                .get_program()
                .to_string_lossy()
        );
        assert_eq!(
            env_value(&command, "GIT_TERMINAL_PROMPT"),
            Some(OsString::from("0"))
        );
        assert_eq!(
            env_value(&command, "GIT_CONFIG_VALUE_0"),
            Some(OsString::from("!gh auth git-credential"))
        );
        assert!(
            command
                .get_args()
                .all(|arg| !arg.to_string_lossy().contains("token"))
        );
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

    #[test]
    fn classifies_only_recoverable_token_helper_failures_as_transient() {
        let reset = GhPrepareError::HelperFailed {
            program: "helper".to_owned(),
            status: Some(1),
            stderr: "GitHub API request failed: [Errno 54] Connection reset by peer".to_owned(),
        };
        let unauthorized = GhPrepareError::HelperFailed {
            program: "helper".to_owned(),
            status: Some(1),
            stderr: "HTTP 401: bad credentials".to_owned(),
        };
        let start_reset = GhPrepareError::HelperStart {
            program: "helper".to_owned(),
            source: std::io::Error::from(std::io::ErrorKind::ConnectionReset),
        };
        let missing = GhPrepareError::HelperStart {
            program: "helper".to_owned(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        };

        assert!(reset.is_transient());
        assert!(
            GhPrepareError::HelperTimedOut {
                program: "helper".to_owned(),
                timeout_ms: 1_000,
            }
            .is_transient()
        );
        assert!(start_reset.is_transient());
        assert!(!unauthorized.is_transient());
        assert!(!missing.is_transient());
        assert!(
            !GhPrepareError::MissingTokenEnv {
                name: "TOKEN".to_owned(),
            }
            .is_transient()
        );
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
