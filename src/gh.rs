//! Shared GitHub CLI command boundary and auth resolution.

mod config;

use config::{GhAuthConfig, GhAuthSource};
pub use config::{
    GhAuthPolicy, GhAuthSourceSummary, GhAuthSummary, GhConfigError, GhPrepareError, GhSupervision,
};

use std::collections::HashMap;
use std::env;
use std::fmt::{Debug, Formatter};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::native_executable::{resolve_native_executable_from_path, validate_native_executable};

const DEFAULT_REFRESH_SKEW_SECONDS: u64 = 60;
const GH_TOKEN_ENV: &str = "GH_TOKEN";
const PR_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(15);

/// Auth-aware GitHub CLI command factory.
#[derive(Clone)]
pub struct GhClient {
    auth: GhAuthConfig,
    cache: Arc<Mutex<HashMap<Vec<String>, CachedToken>>>,
    // Some security-sensitive operations must prove one exact helper token's
    // authority before using it. Once pinned, every command prepared by this
    // client uses that same token or fails when its lifetime ends.
    pinned_token: Option<TokenResolution>,
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
        self.pinned_token = None;
        self
    }

    /// Force token-command repository placeholders to use an explicit target.
    pub fn with_repo_override(mut self, slug: &str) -> Result<Self, GhPrepareError> {
        self.repo_override = Some(RepoIdentity::from_exact_slug(slug).ok_or_else(|| {
            GhPrepareError::InvalidRepoSlug {
                slug: slug.to_owned(),
            }
        })?);
        self.pinned_token = None;
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
        let ambient_binary = if auth_policy == GhAuthPolicy::AmbientOnly {
            Some(match binary_override {
                Some(path) => validate_native_executable(path).map_err(|error| {
                    GhPrepareError::InvalidAmbientGhBinary {
                        path: path.to_path_buf(),
                        detail: error.to_string(),
                    }
                })?,
                None => self.resolve_ambient_gh_binary()?,
            })
        } else {
            None
        };
        let binary = ambient_binary.as_deref().or(binary_override);
        let mut command = match supervision {
            GhSupervision::Supervised => crate::supervised::gh_supervised(binary),
            GhSupervision::Unsupervised => binary.map_or_else(|| Command::new("gh"), Command::new),
        };
        command.current_dir(cwd);
        match auth_policy {
            GhAuthPolicy::Default => {
                if let Some(token) = self.resolve_token_with_timeout(cwd, auth_timeout)? {
                    command.env(GH_TOKEN_ENV, token.token);
                }
            }
            GhAuthPolicy::AmbientOnly => {
                command.env_remove(GH_TOKEN_ENV).env_remove("GITHUB_TOKEN");
            }
        }
        Ok(command)
    }

    /// Prepare a direct native `gh` command for operations that may carry a
    /// privileged token. Unlike the general injectable boundary, this never
    /// executes a PATH-resolved script or wrapper.
    pub(crate) fn prepare_privileged_command(
        &self,
        cwd: &Path,
        supervision: GhSupervision,
    ) -> Result<Command, GhPrepareError> {
        self.prepare_privileged_command_inner(cwd, supervision, None)
    }

    /// Prepare a privileged native `gh` command while bounding token-helper
    /// execution for an unattended daemon lane.
    pub(crate) fn prepare_privileged_command_with_auth_timeout(
        &self,
        cwd: &Path,
        supervision: GhSupervision,
        auth_timeout: Duration,
    ) -> Result<Command, GhPrepareError> {
        self.prepare_privileged_command_inner(cwd, supervision, Some(auth_timeout))
    }

    fn prepare_privileged_command_inner(
        &self,
        cwd: &Path,
        supervision: GhSupervision,
        auth_timeout: Option<Duration>,
    ) -> Result<Command, GhPrepareError> {
        let binary = self.resolve_privileged_gh_binary()?;
        let mut command = match supervision {
            GhSupervision::Supervised => crate::supervised::gh_supervised(Some(&binary)),
            GhSupervision::Unsupervised => Command::new(&binary),
        };
        clear_privileged_environment(&mut command, supervision, &binary);
        command
            .current_dir(cwd)
            .env("LC_ALL", "C")
            .env("GH_HOST", "github.com")
            .env("GH_PROMPT_DISABLED", "1");
        if let Some(token) = self.resolve_token_with_timeout(cwd, auth_timeout)? {
            command.env(GH_TOKEN_ENV, token.token);
        }
        Ok(command)
    }

    fn resolve_ambient_gh_binary(&self) -> Result<PathBuf, GhPrepareError> {
        if let Some(path) = self.auth.ambient_gh_binary.as_deref() {
            return validate_native_executable(path).map_err(|error| {
                GhPrepareError::InvalidAmbientGhBinary {
                    path: path.to_path_buf(),
                    detail: error.to_string(),
                }
            });
        }
        resolve_ambient_gh_from_path(env::var_os("PATH").as_deref())
    }

    fn resolve_privileged_gh_binary(&self) -> Result<PathBuf, GhPrepareError> {
        let path = self
            .auth
            .privileged_gh_binary
            .as_deref()
            .ok_or(GhPrepareError::PrivilegedGhBinaryNotConfigured)?;
        validate_native_executable(path).map_err(|error| {
            GhPrepareError::InvalidPrivilegedGhBinary {
                path: path.to_path_buf(),
                detail: error.to_string(),
            }
        })
    }

    fn resolve_privileged_git_binary(&self) -> Result<PathBuf, GhPrepareError> {
        let path = self
            .auth
            .privileged_git_binary
            .as_deref()
            .ok_or(GhPrepareError::PrivilegedGitBinaryNotConfigured)?;
        validate_native_executable(path).map_err(|error| {
            GhPrepareError::InvalidPrivilegedGitBinary {
                path: path.to_path_buf(),
                detail: error.to_string(),
            }
        })
    }

    /// Prepare a non-interactive `git` command that uses the same configured
    /// GitHub identity as `prepare_command`.
    ///
    /// Explicit token material is kept in the child environment and is never
    /// placed in argv or a remote URL. Token-bearing commands use direct native
    /// executables so a repository-controlled PATH shim cannot receive it.
    pub fn prepare_git_command(&self, cwd: &Path) -> Result<Command, GhPrepareError> {
        self.prepare_git_command_with_binary_authority(cwd, self)
    }

    /// Prepare authenticated Git while sourcing the privileged executable
    /// from a separate trusted configuration boundary.
    pub(crate) fn prepare_git_command_with_binary_authority(
        &self,
        cwd: &Path,
        binary_authority: &Self,
    ) -> Result<Command, GhPrepareError> {
        let token = self.resolve_token(cwd)?;
        let mut command = if token.is_some() {
            binary_authority.prepare_privileged_git_command(cwd)?
        } else {
            let mut command = crate::supervised::git_supervised();
            command.current_dir(cwd);
            command
        };
        let credential_helper = if token.is_some() {
            token_environment_credential_helper()
        } else {
            "!gh auth git-credential"
        };
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "7")
            .env("GIT_CONFIG_KEY_0", "credential.helper")
            .env("GIT_CONFIG_VALUE_0", "")
            .env("GIT_CONFIG_KEY_1", "credential.helper")
            .env("GIT_CONFIG_VALUE_1", credential_helper)
            .env("GIT_CONFIG_KEY_2", "credential.interactive")
            .env("GIT_CONFIG_VALUE_2", "never")
            .env("GIT_CONFIG_KEY_3", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_3", null_device())
            .env("GIT_CONFIG_KEY_4", "protocol.allow")
            .env("GIT_CONFIG_VALUE_4", "never")
            .env("GIT_CONFIG_KEY_5", "protocol.https.allow")
            .env("GIT_CONFIG_VALUE_5", "always")
            .env("GIT_CONFIG_KEY_6", "http.followRedirects")
            .env("GIT_CONFIG_VALUE_6", "false");
        if let Some(token) = token {
            command.env(GH_TOKEN_ENV, token.token);
        } else {
            command.env_remove(GH_TOKEN_ENV).env_remove("GITHUB_TOKEN");
        }
        Ok(command)
    }

    /// Prepare the configured trusted native Git without credentials.
    ///
    /// Dependency publication uses this for every local object and worktree
    /// operation. System/global/inherited Git configuration is excluded; the
    /// caller must use a Shipyard-created repository whose local config is not
    /// consumer-controlled.
    pub(crate) fn prepare_privileged_git_command(
        &self,
        cwd: &Path,
    ) -> Result<Command, GhPrepareError> {
        let git = self.resolve_privileged_git_binary()?;
        let mut command = crate::supervised::supervised(Command::new(&git));
        clear_privileged_environment(&mut command, GhSupervision::Supervised, &git);
        command
            .current_dir(cwd)
            .env("LC_ALL", "C")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "1")
            .env("GIT_CONFIG_KEY_0", "core.hooksPath")
            .env("GIT_CONFIG_VALUE_0", null_device());
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

    /// Resolve a command-helper token once and retain that exact credential
    /// for all later API and Git commands prepared by this client.
    ///
    /// Callers can validate the returned sanitized summary before granting
    /// authority without a time-of-check/time-of-use helper re-resolution.
    pub(crate) fn pin_command_auth(&mut self, cwd: &Path) -> Result<GhAuthSummary, GhPrepareError> {
        self.pin_command_auth_inner(cwd, None)
    }

    /// Pin command auth while bounding helper execution for unattended work.
    pub(crate) fn pin_command_auth_with_timeout(
        &mut self,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<GhAuthSummary, GhPrepareError> {
        self.pin_command_auth_inner(cwd, Some(timeout))
    }

    fn pin_command_auth_inner(
        &mut self,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<GhAuthSummary, GhPrepareError> {
        let GhAuthSource::Command { .. } = &self.auth.source else {
            return self.auth_summary(cwd, GhAuthPolicy::Default);
        };
        let token = self
            .resolve_token_with_timeout(cwd, timeout)?
            .expect("command auth should resolve a token");
        let summary = GhAuthSummary {
            source: GhAuthSourceSummary::Command,
            token_kind: token.kind.clone(),
            expires_at: token.expires_at,
        };
        self.pinned_token = Some(token);
        Ok(summary)
    }

    fn new(auth: GhAuthConfig) -> Self {
        Self {
            auth,
            cache: Arc::new(Mutex::new(HashMap::new())),
            pinned_token: None,
            repo_hint: None,
            repo_override: None,
        }
    }

    fn resolve_token(&self, cwd: &Path) -> Result<Option<TokenResolution>, GhPrepareError> {
        self.resolve_token_with_timeout(cwd, None)
    }

    /// Resolve configured token material for a child process that cannot use
    /// `gh` directly (for example the self-updater's verified installer).
    ///
    /// The caller must keep the returned value out of argv and logs and place
    /// it only in the child's environment. Ambient `gh-cli` auth intentionally
    /// returns `None`; callers may then use their existing best-effort ambient
    /// fallback without weakening a configured env/command source.
    pub(crate) fn resolve_token_for_child(
        &self,
        cwd: &Path,
        timeout: Duration,
    ) -> Result<Option<String>, GhPrepareError> {
        self.resolve_token_with_timeout(cwd, Some(timeout))
            .map(|token| token.map(|token| token.token))
    }

    fn resolve_token_with_timeout(
        &self,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<Option<TokenResolution>, GhPrepareError> {
        if let Some(token) = &self.pinned_token {
            if token
                .valid_until
                .is_some_and(|valid_until| valid_until <= Utc::now())
            {
                return Err(GhPrepareError::TokenExpired);
            }
            return Ok(Some(token.clone()));
        }
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
        Ok(cache
            .get(key)
            .filter(|cached| cached.is_valid(now))
            .map(|cached| cached.token.clone()))
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
        cache.retain(|_, cached| cached.is_valid(now));
        cache.insert(
            key,
            CachedToken {
                token: token.clone(),
                valid_until,
            },
        );
        Ok(())
    }
}

/// Validate an explicit GitHub repository identity without consulting remotes.
pub fn validate_repo_slug(slug: &str) -> Result<(), GhPrepareError> {
    RepoIdentity::from_exact_slug(slug)
        .map(|_| ())
        .ok_or_else(|| GhPrepareError::InvalidRepoSlug {
            slug: slug.to_owned(),
        })
}

fn resolve_ambient_gh_from_path(path: Option<&std::ffi::OsStr>) -> Result<PathBuf, GhPrepareError> {
    resolve_native_gh_from_path(path).ok_or(GhPrepareError::AmbientGhBinaryNotFound)
}

fn resolve_native_gh_from_path(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let executable = format!("gh{}", env::consts::EXE_SUFFIX);
    resolve_native_executable_from_path(&executable, path)
}

fn token_environment_credential_helper() -> &'static str {
    "!f() { test \"$1\" = get || exit 0; protocol=; host=; while IFS= read -r line; do case \"$line\" in protocol=*) protocol=${line#protocol=} ;; host=*) host=${line#host=} ;; esac; done; test \"$protocol\" = https && test \"$host\" = github.com || exit 1; printf '%s\\n' username=x-access-token password=\"$GH_TOKEN\"; }; f"
}

const fn null_device() -> &'static str {
    if cfg!(windows) { "NUL" } else { "/dev/null" }
}

fn clear_privileged_environment(command: &mut Command, supervision: GhSupervision, binary: &Path) {
    // All auth-bearing children start from a closed environment. An explicit
    // deny-list inevitably misses a platform loader, proxy, CA, tracing, or
    // tool-specific override; an allow-list keeps native-path validation from
    // being bypassed before `main` and keeps spawned helper shells equally
    // constrained.
    command.env_clear();
    if supervision == GhSupervision::Supervised {
        command.env(
            crate::supervised::SUPERVISED_ENV_VAR,
            crate::supervised::SUPERVISED_ENV_VALUE,
        );
    }
    #[cfg(not(windows))]
    let _ = binary;
    #[cfg(windows)]
    if let Some(windows) = known_folders::get_known_folder_path(known_folders::KnownFolder::Windows)
    {
        let system = windows.join("System32");
        command
            .env("SYSTEMROOT", &windows)
            .env("WINDIR", &windows)
            .env("COMSPEC", system.join("cmd.exe"));
        let mut path = vec![system];
        if let Some(parent) = binary.parent() {
            path.insert(0, parent.to_path_buf());
        }
        if let Ok(path) = env::join_paths(path) {
            command.env("PATH", path);
        }
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
    installation_id: Option<u64>,
    expires_at: Option<DateTime<Utc>>,
    valid_until: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CachedToken {
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
        installation_id: None,
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
        kind: inferred_token_kind(trimmed),
        installation_id: None,
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
        .map(ToOwned::to_owned)
        .or_else(|| inferred_token_kind(&token));
    let expires_at = match value.get("expires_at") {
        Some(Value::String(expires_at)) => Some(
            DateTime::parse_from_rfc3339(expires_at)
                .map_err(|_| GhPrepareError::HelperStdoutMalformed)?
                .with_timezone(&Utc),
        ),
        Some(Value::Null) | None => None,
        Some(_) => return Err(GhPrepareError::HelperStdoutMalformed),
    };
    let installation_id = match value.get("installation_id") {
        Some(Value::String(value)) => value
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or(GhPrepareError::HelperStdoutMalformed)
            .map(Some)?,
        Some(Value::Number(value)) => value
            .as_u64()
            .filter(|value| *value > 0)
            .ok_or(GhPrepareError::HelperStdoutMalformed)
            .map(Some)?,
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
        installation_id,
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

impl GhClient {
    pub(crate) fn app_installation_id(&self, cwd: &Path) -> Result<u64, GhPrepareError> {
        // Installation access tokens are opaque and GitHub exposes no
        // token-authenticated endpoint that returns their installation ID.
        // The machine-global command helper is therefore the trusted mint
        // boundary: it resolves the repository installation with an App JWT,
        // mints that installation's token, and returns both in one response.
        // Callers separately use the token to re-read the exact repository/PR.
        let token = self
            .resolve_token_with_timeout(cwd, Some(Duration::from_secs(15)))?
            .ok_or(GhPrepareError::HelperStdoutMalformed)?;
        if token.kind.as_deref() != Some("github-app-installation") {
            return Err(GhPrepareError::HelperStdoutMalformed);
        }
        token
            .installation_id
            .ok_or(GhPrepareError::HelperStdoutMalformed)
    }
}

fn inferred_token_kind(token: &str) -> Option<String> {
    // GitHub installation access tokens use the documented `ghs_` prefix.
    // Recording the sanitized kind lets security-sensitive callers require
    // App authority even when an existing helper emits the traditional plain
    // token form instead of Shipyard's optional JSON envelope.
    token
        .starts_with("ghs_")
        .then(|| "github-app-installation".to_owned())
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

    /// Build from a canonical CLI `OWNER/REPO` value. Unlike remote parsing,
    /// this rejects whitespace and URL/ref-like punctuation rather than
    /// normalizing user input.
    fn from_exact_slug(slug: &str) -> Option<Self> {
        if slug != slug.trim() {
            return None;
        }
        let (owner, name) = slug.split_once('/')?;
        if name.contains('/')
            || !(1..=39).contains(&owner.len())
            || !owner
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            || !owner
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || matches!(name, "" | "." | "..")
            || name.len() > 255
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
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
    let remotes_output = Command::new("git")
        .arg("remote")
        .current_dir(cwd)
        .output()
        .map_err(|source| GhPrepareError::RepoProbeFailed { source })?;
    if !remotes_output.status.success() {
        return Err(GhPrepareError::RepoSlugRequired);
    }
    let remotes: Vec<_> = String::from_utf8_lossy(&remotes_output.stdout)
        .lines()
        .filter(|remote| !remote.is_empty())
        .map(str::to_owned)
        .collect();
    let mut resolved = Vec::new();
    for remote in &remotes {
        let key = format!("remote.{remote}.gh-resolved");
        let output = Command::new("git")
            .args(["config", "--get", &key])
            .current_dir(cwd)
            .output()
            .map_err(|source| GhPrepareError::RepoProbeFailed { source })?;
        if output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "base" {
            resolved.push(remote.as_str());
        }
    }
    let remote_name = match (resolved.as_slice(), remotes.as_slice()) {
        ([remote], _) => *remote,
        ([], [remote]) => remote.as_str(),
        ([], []) => return Err(GhPrepareError::RepoSlugRequired),
        _ => return Err(GhPrepareError::RepoRemoteAmbiguous),
    };
    let key = format!("remote.{remote_name}.url");
    let output = Command::new("git")
        .args(["config", "--get", &key])
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
mod tests;
