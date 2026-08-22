use std::error::Error;
use std::fmt::{Display, Formatter};
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::config::{ConfigLoadError, LoadedConfig};

use super::{DEFAULT_REFRESH_SKEW_SECONDS, helper_failure_is_transient, redact_token_like_text};

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
pub(super) struct GhAuthConfig {
    pub(super) source: GhAuthSource,
    pub(super) refresh_skew_seconds: u64,
    pub(super) ambient_gh_binary: Option<PathBuf>,
    pub(super) privileged_gh_binary: Option<PathBuf>,
    pub(super) privileged_git_binary: Option<PathBuf>,
}

impl GhAuthConfig {
    pub(super) fn ambient() -> Self {
        Self {
            source: GhAuthSource::GhCli,
            refresh_skew_seconds: DEFAULT_REFRESH_SKEW_SECONDS,
            ambient_gh_binary: None,
            privileged_gh_binary: None,
            privileged_git_binary: None,
        }
    }

    pub(super) fn from_loaded_config(config: &LoadedConfig) -> Result<Self, GhConfigError> {
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
pub(super) enum GhAuthSource {
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
    ambient_gh_binary: Option<PathBuf>,
    privileged_gh_binary: Option<PathBuf>,
    privileged_git_binary: Option<PathBuf>,
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
        if self
            .ambient_gh_binary
            .as_ref()
            .is_some_and(|path| !path.is_absolute())
        {
            return Err(GhConfigError::Invalid {
                message: "`github.auth.ambient_gh_binary` must be an absolute path".to_owned(),
            });
        }
        for (key, path) in [
            (
                "github.auth.privileged_gh_binary",
                self.privileged_gh_binary.as_ref(),
            ),
            (
                "github.auth.privileged_git_binary",
                self.privileged_git_binary.as_ref(),
            ),
        ] {
            if path.is_some_and(|path| !path.is_absolute()) {
                return Err(GhConfigError::Invalid {
                    message: format!("`{key}` must be an absolute path"),
                });
            }
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
            ambient_gh_binary: self.ambient_gh_binary,
            privileged_gh_binary: self.privileged_gh_binary,
            privileged_git_binary: self.privileged_git_binary,
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
    /// No direct native `gh` executable was available for ambient auth.
    AmbientGhBinaryNotFound,
    /// An explicitly configured ambient `gh` path was not a usable native executable.
    InvalidAmbientGhBinary {
        /// Rejected path.
        path: PathBuf,
        /// Sanitized validation failure.
        detail: String,
    },
    /// No trusted direct native `gh` executable was configured for a token-bearing command.
    PrivilegedGhBinaryNotConfigured,
    /// A token-bearing command was given a non-native `gh` executable.
    InvalidPrivilegedGhBinary {
        /// Rejected path.
        path: PathBuf,
        /// Sanitized validation failure.
        detail: String,
    },
    /// No trusted direct native `git` executable was configured for a token-bearing command.
    PrivilegedGitBinaryNotConfigured,
    /// A token-bearing command was given a non-native `git` executable.
    InvalidPrivilegedGitBinary {
        /// Rejected path.
        path: PathBuf,
        /// Sanitized validation failure.
        detail: String,
    },
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
            Self::AmbientGhBinaryNotFound => write!(
                f,
                "ambient gh fallback could not find a native gh executable on PATH; install GitHub CLI or set github.auth.ambient_gh_binary to its absolute path"
            ),
            Self::InvalidAmbientGhBinary { path, detail } => write!(
                f,
                "configured ambient gh binary {} is invalid: {detail}",
                path.display()
            ),
            Self::PrivilegedGhBinaryNotConfigured => write!(
                f,
                "token-bearing GitHub command requires github.auth.privileged_gh_binary to name a trusted absolute native gh executable"
            ),
            Self::InvalidPrivilegedGhBinary { path, detail } => write!(
                f,
                "token-bearing GitHub command rejected gh binary {}: {detail}",
                path.display()
            ),
            Self::PrivilegedGitBinaryNotConfigured => write!(
                f,
                "token-bearing Git command requires github.auth.privileged_git_binary to name a trusted absolute native git executable"
            ),
            Self::InvalidPrivilegedGitBinary { path, detail } => write!(
                f,
                "token-bearing Git command rejected git binary {}: {detail}",
                path.display()
            ),
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
            | Self::AmbientGhBinaryNotFound
            | Self::InvalidAmbientGhBinary { .. }
            | Self::PrivilegedGhBinaryNotConfigured
            | Self::InvalidPrivilegedGhBinary { .. }
            | Self::PrivilegedGitBinaryNotConfigured
            | Self::InvalidPrivilegedGitBinary { .. }
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
