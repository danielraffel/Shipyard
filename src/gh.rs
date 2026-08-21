//! Shared GitHub CLI command boundary and auth resolution.

mod config;

use config::{GhAuthConfig, GhAuthSource};
pub use config::{
    GhAuthPolicy, GhAuthSourceSummary, GhAuthSummary, GhConfigError, GhPrepareError, GhSupervision,
};

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

fn resolve_ambient_gh_from_path(path: Option<&std::ffi::OsStr>) -> Result<PathBuf, GhPrepareError> {
    let executable = format!("gh{}", env::consts::EXE_SUFFIX);
    resolve_native_executable_from_path(&executable, path)
        .ok_or(GhPrepareError::AmbientGhBinaryNotFound)
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
