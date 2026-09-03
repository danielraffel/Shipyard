//! GitHub webhook registration through the user's existing `gh` auth.
//!
//! The registrar mirrors the Python daemon contract: hook IDs are persisted
//! under `daemon/registrations.json`, restarts patch existing hooks instead of
//! creating duplicates, and shutdown best-effort unregisters known hooks.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::json;
use wait_timeout::ChildExt;

use crate::daemon_ipc::rate_limit_is_anonymous;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;

/// GitHub webhook events Shipyard subscribes to.
pub const SUBSCRIBED_EVENTS: [&str; 6] = [
    "workflow_run",
    "workflow_job",
    "pull_request",
    "check_run",
    "check_suite",
    "release",
];

/// One-time GitHub CLI remediation for managing repository webhooks.
pub const WEBHOOK_SCOPE_COMMAND: &str = "gh auth refresh -h github.com -s admin:repo_hook";

const GH_API_TIMEOUT: Duration = Duration::from_secs(15);

/// Durable repo-to-hook mapping.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegisteredHook {
    /// Repository in `owner/name` form.
    pub repo: String,
    /// GitHub webhook ID.
    pub hook_id: u64,
}

/// Non-recoverable registrar failure.
#[derive(Debug)]
pub enum RegistrarError {
    /// Filesystem or process boundary failed.
    Io(std::io::Error),
    /// The configured `gh` binary is missing or not executable.
    GhUnavailable(String),
    /// A `gh api` invocation exceeded the registrar timeout.
    GhTimedOut,
    /// GitHub CLI returned a non-zero status.
    GhFailed {
        /// Registrar operation being attempted.
        action: &'static str,
        /// Combined stdout/stderr from `gh`.
        output: String,
    },
    /// GitHub CLI lacks the admin scope required to manage repo hooks.
    MissingWebhookScope {
        /// Registrar operation being attempted.
        action: &'static str,
        /// Combined stdout/stderr from `gh`.
        output: String,
    },
    /// GitHub rejected the request because the token isn't authenticating:
    /// HTTP 401/403 (bad/expired/missing credentials) or the anonymous
    /// 60-req/hr rate limit. Distinct from a missing repo-hook scope — the
    /// token itself is invalid, so live updates degrade until it's fixed.
    AuthDegraded {
        /// Registrar operation being attempted.
        action: &'static str,
        /// Combined stdout/stderr from `gh`.
        output: String,
    },
    /// The repository or hook endpoint no longer exists. Callers may
    /// reconcile by listing remote hooks before deciding whether to create.
    RemoteNotFound {
        action: &'static str,
        output: String,
    },
    /// GitHub or the network returned a retryable response. Polling remains
    /// the source of truth; do not persist a partial registration.
    Transient {
        action: &'static str,
        output: String,
    },
    /// GitHub CLI returned a successful response without a hook ID.
    MissingHookId(String),
    /// GitHub returned more than one Shipyard hook for the exact callback URL.
    /// Adopting either would make local provenance ambiguous, so fail closed.
    AmbiguousRemoteHooks {
        /// Repository whose hook provenance is ambiguous.
        repo: String,
        /// Exact callback URL used to identify Shipyard's hook.
        url: String,
        /// Matching GitHub hook IDs.
        hook_ids: Vec<u64>,
    },
    /// GitHub accepted a hook PATCH but did not return the complete requested
    /// subscription. Local provenance must not be committed for partial state.
    HookReconciliationMismatch {
        /// Hook whose returned state was incomplete.
        hook_id: u64,
        /// Non-secret mismatch description.
        detail: String,
    },
    /// Persisted registration state could not be serialized or parsed.
    Json(serde_json::Error),
}

impl std::fmt::Display for RegistrarError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "{error}"),
            Self::GhUnavailable(message) => formatter.write_str(message),
            Self::GhTimedOut => formatter.write_str("gh api timed out"),
            Self::GhFailed { action, output } => {
                write!(formatter, "{action} hook failed: {}", output.trim())
            }
            Self::MissingWebhookScope { action, output } => {
                write!(
                    formatter,
                    "{action} hook failed: {}. Run `{WEBHOOK_SCOPE_COMMAND}` and restart Shipyard.",
                    output.trim()
                )
            }
            Self::AuthDegraded { action, output } => {
                write!(
                    formatter,
                    "{action} hook failed: GitHub rejected the request ({}). The token is invalid, expired, or missing. Run `gh auth status` or configure a [github.auth] token.",
                    output.trim()
                )
            }
            Self::RemoteNotFound { action, output } => {
                write!(formatter, "{action} hook target not found: {}", output.trim())
            }
            Self::Transient { action, output } => {
                write!(formatter, "{action} hook temporarily unavailable: {}", output.trim())
            }
            Self::MissingHookId(output) => {
                write!(
                    formatter,
                    "couldn't parse hook ID from gh response: {}",
                    output.trim()
                )
            }
            Self::AmbiguousRemoteHooks {
                repo,
                url,
                hook_ids,
            } => write!(
                formatter,
                "refusing to adopt ambiguous webhook provenance for {repo}: exact URL {url} matches hook IDs {hook_ids:?}"
            ),
            Self::HookReconciliationMismatch { hook_id, detail } => write!(
                formatter,
                "GitHub hook {hook_id} did not confirm the complete requested subscription: {detail}"
            ),
            Self::Json(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RegistrarError {}

impl RegistrarError {
    /// True when remediation is the GitHub webhook management scope refresh.
    #[must_use]
    pub fn is_missing_webhook_scope(&self) -> bool {
        matches!(self, Self::MissingWebhookScope { .. })
    }

    /// True when GitHub rejected the request because the token isn't
    /// authenticating (HTTP 401/403 or the anonymous 60/hr rate limit).
    #[must_use]
    pub fn is_auth_degraded(&self) -> bool {
        matches!(self, Self::AuthDegraded { .. })
    }

    /// Concise, human detail for an auth-degraded failure, suitable as the
    /// trailing text of a `github_auth_degraded:` pause message. Empty for
    /// other error kinds.
    #[must_use]
    pub fn auth_degraded_detail(&self) -> String {
        match self {
            Self::AuthDegraded { output, .. } => auth_failure_detail(output),
            _ => String::new(),
        }
    }

    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(self, Self::RemoteNotFound { .. })
    }

    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient { .. })
    }
}

impl From<std::io::Error> for RegistrarError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for RegistrarError {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// Persistent GitHub webhook registrar.
#[derive(Clone, Debug)]
pub struct Registrar {
    state_path: PathBuf,
    by_repo: BTreeMap<String, u64>,
    #[cfg_attr(test, allow(dead_code))]
    mode: RuntimeMode,
    cwd: PathBuf,
}

impl Registrar {
    /// Load registrar state from `<state_dir>/daemon/registrations.json`.
    #[must_use]
    pub fn new(state_dir: &Path) -> Self {
        Self::new_with_context(
            RuntimeMode::Shipyard,
            state_dir,
            &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        )
    }

    /// Load registrar state with explicit runtime context for configured auth.
    #[must_use]
    pub fn new_with_context(mode: RuntimeMode, state_dir: &Path, cwd: &Path) -> Self {
        let state_path = state_dir.join("daemon").join("registrations.json");
        let by_repo = load_registrations(&state_path);
        Self {
            state_path,
            by_repo,
            mode,
            cwd: cwd.to_path_buf(),
        }
    }

    /// Return the current repo-to-hook map.
    #[must_use]
    pub fn all(&self) -> BTreeMap<String, u64> {
        self.by_repo.clone()
    }

    /// Idempotently create or update a webhook using `gh` from `PATH`.
    pub fn ensure_registered(
        &mut self,
        repo: &str,
        url: &str,
        secret: &str,
    ) -> Result<u64, RegistrarError> {
        let repo = canonical_repo(repo);
        let client = self.configured_gh_client(&repo)?;
        self.ensure_registered_with_client(&repo, url, secret, &client, None)
    }

    /// Idempotently create or update a webhook with an explicit `gh` binary.
    pub fn ensure_registered_with_gh(
        &mut self,
        repo: &str,
        url: &str,
        secret: &str,
        gh_binary: &Path,
    ) -> Result<u64, RegistrarError> {
        validate_gh_binary(gh_binary)?;
        let repo = canonical_repo(repo);
        let client = GhClient::ambient();
        self.ensure_registered_with_client(&repo, url, secret, &client, Some(gh_binary))
    }

    fn ensure_registered_with_client(
        &mut self,
        repo: &str,
        url: &str,
        secret: &str,
        client: &GhClient,
        gh_binary: Option<&Path>,
    ) -> Result<u64, RegistrarError> {
        if let Some(hook_id) = self.by_repo.get(repo).copied() {
            match update_hook(client, &self.cwd, gh_binary, repo, hook_id, url, secret) {
                Ok(()) => return Ok(hook_id),
                Err(RegistrarError::RemoteNotFound { .. }) => {
                    // The persisted remote hook was deleted out of band.
                    // Drop only this stale binding and reconcile by URL below.
                    self.by_repo.remove(repo);
                }
                Err(error) => return Err(error),
            }
        }

        let matching = list_matching_hooks(client, &self.cwd, gh_binary, repo, url)?;
        let hook_id = match matching.as_slice() {
            [] => create_hook(client, &self.cwd, gh_binary, repo, url, secret)?,
            [hook_id] => {
                update_hook(client, &self.cwd, gh_binary, repo, *hook_id, url, secret)?;
                *hook_id
            }
            _ => {
                return Err(RegistrarError::AmbiguousRemoteHooks {
                    repo: repo.to_owned(),
                    url: url.to_owned(),
                    hook_ids: matching,
                });
            }
        };
        self.by_repo.insert(repo.to_owned(), hook_id);
        self.save()?;
        Ok(hook_id)
    }

    /// Best-effort unregister a repo using `gh` from `PATH` when present.
    pub fn unregister(&mut self, repo: &str) -> Result<(), RegistrarError> {
        let repo = canonical_repo(repo);
        let Some(hook_id) = self.by_repo.get(&repo).copied() else {
            return Ok(());
        };
        if let Some(client) = self.configured_gh_client_optional(Some(&repo))? {
            delete_hook(&client, &self.cwd, None, &repo, hook_id)?;
        }
        self.by_repo.remove(&repo);
        self.save()
    }

    /// Unregister a repo with an explicit `gh` binary.
    pub fn unregister_with_gh(
        &mut self,
        repo: &str,
        gh_binary: &Path,
    ) -> Result<(), RegistrarError> {
        let repo = canonical_repo(repo);
        let Some(hook_id) = self.by_repo.get(&repo).copied() else {
            return Ok(());
        };
        validate_gh_binary(gh_binary)?;
        let client = GhClient::ambient();
        delete_hook(&client, &self.cwd, Some(gh_binary), &repo, hook_id)?;
        self.by_repo.remove(&repo);
        self.save()
    }

    /// Best-effort unregister every known repo.
    pub fn unregister_all(&mut self) -> Result<(), RegistrarError> {
        for repo in self.by_repo.keys().cloned().collect::<Vec<_>>() {
            self.unregister(&repo)?;
        }
        Ok(())
    }

    fn save(&self) -> Result<(), RegistrarError> {
        let _writer_domain =
            crate::writer_domain_lease::acquire_for_protected_path(&self.state_path)?;
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = self
            .by_repo
            .iter()
            .map(|(repo, hook_id)| RegistrationRecord {
                repo: repo.clone(),
                hook_id: *hook_id,
            })
            .collect::<Vec<_>>();
        let encoded = serde_json::to_vec_pretty(&payload)?;
        let parent = self.state_path.parent().ok_or_else(|| {
            RegistrarError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "registrations path has no parent",
            ))
        })?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&encoded)?;
        temporary.flush()?;
        temporary
            .persist(&self.state_path)
            .map_err(|error| RegistrarError::Io(error.error))?;
        Ok(())
    }

    fn configured_gh_client(&self, repo: &str) -> Result<GhClient, RegistrarError> {
        self.configured_gh_client_optional(Some(repo))?
            .ok_or_else(|| RegistrarError::GhUnavailable("gh CLI not found on PATH".to_owned()))
    }

    /// Build the configured `gh` client, hinting `repo` for a `{repo_slug}`
    /// token-command placeholder (the daemon's CWD isn't a GitHub checkout).
    #[allow(clippy::unused_self, clippy::unnecessary_wraps)]
    fn configured_gh_client_optional(
        &self,
        repo: Option<&str>,
    ) -> Result<Option<GhClient>, RegistrarError> {
        #[cfg(test)]
        {
            let _ = repo;
            Ok(None)
        }
        #[cfg(not(test))]
        {
            let client = GhClient::from_cwd(self.mode, &self.cwd)
                .map_err(|error| RegistrarError::GhUnavailable(error.to_string()))?;
            let client = match repo {
                Some(slug) => client.with_repo_hint(slug),
                None => client,
            };
            Ok(Some(client))
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct RegistrationRecord {
    repo: String,
    hook_id: u64,
}

fn load_registrations(state_path: &Path) -> BTreeMap<String, u64> {
    let Ok(raw) = fs::read_to_string(state_path) else {
        return BTreeMap::new();
    };
    let Ok(records) = serde_json::from_str::<Vec<RegistrationRecord>>(&raw) else {
        return BTreeMap::new();
    };
    records
        .into_iter()
        .filter(|record| !record.repo.trim().is_empty())
        .map(|record| (canonical_repo(&record.repo), record.hook_id))
        .collect()
}

/// Canonical repository identity used for durable registrar keys and lookups.
/// GitHub repository slugs are case-insensitive; lowercase prevents duplicate
/// registrations when callers use mixed-case owner/name spellings.
fn canonical_repo(repo: &str) -> String {
    repo.trim().to_ascii_lowercase()
}

fn create_hook(
    client: &GhClient,
    cwd: &Path,
    gh_binary: Option<&Path>,
    repo: &str,
    url: &str,
    secret: &str,
) -> Result<u64, RegistrarError> {
    let body = json!({
        "name": "web",
        "active": true,
        "events": SUBSCRIBED_EVENTS,
        "config": {
            "url": url,
            "content_type": "json",
            "secret": secret,
            "insecure_ssl": "0",
        },
    });
    let output = run_gh(
        client,
        cwd,
        gh_binary,
        &[
            "api",
            "-X",
            "POST",
            "-H",
            "Accept: application/vnd.github+json",
            "--input",
            "-",
            &format!("repos/{repo}/hooks"),
        ],
        Some(&body.to_string()),
    )?;
    if output.status != 0 {
        return Err(classify_gh_failure("create", output.combined_output()));
    }
    let parsed = serde_json::from_str::<serde_json::Value>(&output.stdout)
        .map_err(|_| RegistrarError::MissingHookId(output.stdout.clone()))?;
    parsed
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or(RegistrarError::MissingHookId(output.stdout))
}

fn list_matching_hooks(
    client: &GhClient,
    cwd: &Path,
    gh_binary: Option<&Path>,
    repo: &str,
    url: &str,
) -> Result<Vec<u64>, RegistrarError> {
    let output = run_gh(
        client,
        cwd,
        gh_binary,
        &[
            "api",
            "--paginate",
            "--slurp",
            "-H",
            "Accept: application/vnd.github+json",
            &format!("repos/{repo}/hooks?per_page=100"),
        ],
        None,
    )?;
    if output.status != 0 {
        return Err(classify_gh_failure("list", output.combined_output()));
    }
    let pages = serde_json::from_str::<Vec<Vec<serde_json::Value>>>(&output.stdout)?;
    let mut matches = pages
        .into_iter()
        .flatten()
        .filter(|hook| hook.get("name").and_then(serde_json::Value::as_str) == Some("web"))
        .filter(|hook| {
            hook.get("config")
                .and_then(|config| config.get("url"))
                .and_then(serde_json::Value::as_str)
                == Some(url)
        })
        .filter_map(|hook| hook.get("id").and_then(serde_json::Value::as_u64))
        .collect::<Vec<_>>();
    matches.sort_unstable();
    matches.dedup();
    Ok(matches)
}

fn update_hook(
    client: &GhClient,
    cwd: &Path,
    gh_binary: Option<&Path>,
    repo: &str,
    hook_id: u64,
    url: &str,
    secret: &str,
) -> Result<(), RegistrarError> {
    let body = json!({
        "config": {
            "url": url,
            "content_type": "json",
            "secret": secret,
            "insecure_ssl": "0",
        },
        "active": true,
        "events": SUBSCRIBED_EVENTS,
    });
    let output = run_gh(
        client,
        cwd,
        gh_binary,
        &[
            "api",
            "-X",
            "PATCH",
            "-H",
            "Accept: application/vnd.github+json",
            "--input",
            "-",
            &format!("repos/{repo}/hooks/{hook_id}"),
        ],
        Some(&body.to_string()),
    )?;
    if output.status != 0 {
        return Err(classify_gh_failure("patch", output.combined_output()));
    }
    validate_updated_hook(hook_id, url, &output.stdout)
}

fn validate_updated_hook(hook_id: u64, url: &str, output: &str) -> Result<(), RegistrarError> {
    let value = serde_json::from_str::<serde_json::Value>(output)?;
    let returned_url = value
        .get("config")
        .and_then(|config| config.get("url"))
        .and_then(serde_json::Value::as_str);
    if returned_url != Some(url) {
        return Err(RegistrarError::HookReconciliationMismatch {
            hook_id,
            detail: "callback URL differs".to_owned(),
        });
    }
    if value.get("active").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(RegistrarError::HookReconciliationMismatch {
            hook_id,
            detail: "hook is not active".to_owned(),
        });
    }
    let Some(events) = value.get("events").and_then(serde_json::Value::as_array) else {
        return Err(RegistrarError::HookReconciliationMismatch {
            hook_id,
            detail: "events are missing".to_owned(),
        });
    };
    let mut actual = events
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<Vec<_>>();
    actual.sort_unstable();
    let mut expected = SUBSCRIBED_EVENTS.to_vec();
    expected.sort_unstable();
    if events.len() != expected.len() || actual != expected {
        return Err(RegistrarError::HookReconciliationMismatch {
            hook_id,
            detail: format!("events differ: expected {expected:?}, got {actual:?}"),
        });
    }
    Ok(())
}

fn delete_hook(
    client: &GhClient,
    cwd: &Path,
    gh_binary: Option<&Path>,
    repo: &str,
    hook_id: u64,
) -> Result<(), RegistrarError> {
    let output = run_gh(
        client,
        cwd,
        gh_binary,
        &[
            "api",
            "-X",
            "DELETE",
            &format!("repos/{repo}/hooks/{hook_id}"),
        ],
        None,
    )?;
    if output.status == 0 {
        return Ok(());
    }
    let combined = output.combined_output();
    let lowered = combined.to_ascii_lowercase();
    if lowered.contains("404") || lowered.contains("not found") {
        return Ok(());
    }
    Err(classify_gh_failure("delete", combined))
}

fn classify_gh_failure(action: &'static str, output: String) -> RegistrarError {
    // Order matters: a missing repo-hook scope is a 403 too, but it's a
    // one-time grant, not a dead token — keep it distinct and check first.
    if mentions_webhook_scope(&output) {
        RegistrarError::MissingWebhookScope { action, output }
    } else if mentions_auth_failure(&output) {
        RegistrarError::AuthDegraded { action, output }
    } else if mentions_not_found(&output) {
        RegistrarError::RemoteNotFound { action, output }
    } else if mentions_transient(&output) {
        RegistrarError::Transient { action, output }
    } else {
        RegistrarError::GhFailed { action, output }
    }
}

fn mentions_not_found(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    lowered.contains("http 404") || lowered.contains("404 not found") || lowered.contains("not found")
}

fn mentions_transient(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    ["http 408", "http 409", "http 429", "http 500", "http 502", "http 503", "http 504"]
        .iter()
        .any(|status| lowered.contains(status))
        || lowered.contains("timed out")
        || lowered.contains("temporarily unavailable")
}

fn mentions_webhook_scope(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    lowered.contains("admin:repo_hook") || lowered.contains("repo_hook")
}

/// True when `gh api` output indicates the token isn't authenticating — an
/// HTTP 401/403, an explicit bad/expired-credentials message, or GitHub's
/// anonymous (unauthenticated) rate-limit response. The anonymous rate limit is
/// recognized either from the human "API rate limit exceeded" message or, when
/// `gh` echoes a `rate_limit` JSON body, from a core limit of 60 via
/// [`rate_limit_is_anonymous`].
fn mentions_auth_failure(output: &str) -> bool {
    let lowered = output.to_ascii_lowercase();
    let http_auth_status = lowered.contains("http 401")
        || lowered.contains("http 403")
        || lowered.contains("401 unauthorized")
        || lowered.contains("403 forbidden");
    let credential_hint = lowered.contains("bad credentials")
        || lowered.contains("requires authentication")
        || lowered.contains("must authenticate")
        || lowered.contains("token expired")
        || lowered.contains("invalid token");
    let anonymous_rate_limit = lowered.contains("api rate limit exceeded")
        || serde_json::from_str::<serde_json::Value>(output.trim())
            .is_ok_and(|value| rate_limit_is_anonymous(&value));
    http_auth_status || credential_hint || anonymous_rate_limit
}

/// Build a concise human detail from raw `gh` auth-failure output for the
/// `github_auth_degraded:` pause message. Prefers the anonymous-rate-limit
/// explanation, then the first meaningful line of `gh` output, and finally a
/// generic fallback.
fn auth_failure_detail(output: &str) -> String {
    let lowered = output.to_ascii_lowercase();
    if lowered.contains("api rate limit exceeded")
        || serde_json::from_str::<serde_json::Value>(output.trim())
            .is_ok_and(|value| rate_limit_is_anonymous(&value))
    {
        return "unauthenticated (anonymous 60/hr) — token invalid or missing".to_owned();
    }
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map_or_else(
            || "invalid or expired GitHub token".to_owned(),
            |line| line.chars().take(200).collect(),
        )
}

#[derive(Debug)]
struct GhOutput {
    status: i32,
    stdout: String,
    stderr: String,
}

impl GhOutput {
    fn combined_output(&self) -> String {
        if self.stderr.is_empty() {
            self.stdout.clone()
        } else if self.stdout.is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

fn run_gh(
    client: &GhClient,
    cwd: &Path,
    gh_binary: Option<&Path>,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<GhOutput, RegistrarError> {
    let mut command = client
        .prepare_command(
            cwd,
            gh_binary,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| RegistrarError::GhUnavailable(error.to_string()))?;
    command
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            RegistrarError::GhUnavailable("gh CLI not found on PATH".to_owned())
        } else {
            RegistrarError::Io(error)
        }
    })?;
    if let Some(stdin) = stdin
        && let Some(mut writer) = child.stdin.take()
    {
        writer.write_all(stdin.as_bytes())?;
    }

    let Some(status) = child.wait_timeout(GH_API_TIMEOUT)? else {
        let _ = child.kill();
        let _ = child.wait();
        return Err(RegistrarError::GhTimedOut);
    };
    let output = child.wait_with_output()?;
    Ok(GhOutput {
        status: status.code().unwrap_or(1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

fn validate_gh_binary(path: &Path) -> Result<(), RegistrarError> {
    let metadata = fs::metadata(path).map_err(|_| {
        RegistrarError::GhUnavailable(format!("gh CLI not executable: {}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(RegistrarError::GhUnavailable(format!(
            "gh CLI not executable: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(RegistrarError::GhUnavailable(format!(
                "gh CLI not executable: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    use serde_json::Value;

    use super::Registrar;
    #[cfg(unix)]
    use super::{RegistrarError, RuntimeMode, SUBSCRIBED_EVENTS, WEBHOOK_SCOPE_COMMAND};

    #[test]
    fn corrupt_state_loads_as_empty() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state_path = temp.path().join("daemon").join("registrations.json");
        fs::create_dir_all(state_path.parent().expect("parent")).expect("mkdir");
        fs::write(&state_path, "not json").expect("write");

        let registrar = Registrar::new(temp.path());

        assert!(registrar.all().is_empty());
    }

    #[test]
    fn mixed_case_persisted_repository_is_canonicalized() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("daemon").join("registrations.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, r#"[{"repo":" Generous-Corp/PuLp ","hook_id":17}]"#).expect("write");

        let registrar = Registrar::new(temp.path());
        assert_eq!(registrar.all().get("generous-corp/pulp"), Some(&17));
        assert!(!registrar.all().contains_key("Generous-Corp/PuLp"));
    }

    #[cfg(unix)]
    #[test]
    fn creates_updates_deletes_and_persists_hooks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::Ok);
        let mut registrar = stub_registrar(temp.path());

        let hook_id = registrar
            .ensure_registered_with_gh(
                "owner/repo",
                "https://shipyard.example/webhook",
                "secret-one",
                &gh,
            )
            .expect("create");
        assert_eq!(hook_id, 4242);

        let persisted = fs::read_to_string(temp.path().join("daemon").join("registrations.json"))
            .expect("registrations");
        let records = serde_json::from_str::<Vec<Value>>(&persisted).expect("records");
        assert_eq!(records[0]["repo"], "owner/repo");
        assert_eq!(records[0]["hook_id"], 4242);

        let mut reloaded = stub_registrar(temp.path());
        let hook_id = reloaded
            .ensure_registered_with_gh(
                "owner/repo",
                "https://shipyard.example/rotated/webhook",
                "secret-two",
                &gh,
            )
            .expect("patch");
        assert_eq!(hook_id, 4242);

        reloaded
            .unregister_with_gh("owner/repo", &gh)
            .expect("delete");
        assert!(reloaded.all().is_empty());

        let list_args = read_log(temp.path(), "args-1");
        let first_args = read_log(temp.path(), "args-2");
        let first_body = read_json_log(temp.path(), "stdin-2");
        let second_args = read_log(temp.path(), "args-3");
        let second_body = read_json_log(temp.path(), "stdin-3");
        let third_args = read_log(temp.path(), "args-4");

        assert!(list_args.contains("--paginate --slurp"));
        assert!(list_args.contains("repos/owner/repo/hooks?per_page=100"));
        assert!(first_args.contains("-X POST"));
        assert!(first_args.contains("repos/owner/repo/hooks"));
        assert_eq!(first_body["name"], "web");
        assert_eq!(first_body["active"], true);
        assert_eq!(
            first_body["config"]["url"],
            "https://shipyard.example/webhook"
        );
        assert_eq!(first_body["config"]["content_type"], "json");
        assert_eq!(first_body["config"]["secret"], "secret-one");
        assert_eq!(first_body["config"]["insecure_ssl"], "0");
        let events = first_body["events"].as_array().expect("events");
        assert_eq!(events.len(), SUBSCRIBED_EVENTS.len());
        for event in SUBSCRIBED_EVENTS {
            assert!(events.iter().any(|value| value.as_str() == Some(event)));
        }

        assert!(second_args.contains("-X PATCH"));
        assert!(second_args.contains("repos/owner/repo/hooks/4242"));
        assert_eq!(
            second_body["config"]["url"],
            "https://shipyard.example/rotated/webhook"
        );
        assert_eq!(second_body["config"]["secret"], "secret-two");

        assert!(third_args.contains("-X DELETE"));
        assert!(third_args.contains("repos/owner/repo/hooks/4242"));
    }

    #[cfg(unix)]
    #[test]
    fn mixed_case_alias_reuses_and_unregisters_canonical_registration() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::Ok);
        let mut registrar = stub_registrar(temp.path());
        let id = registrar
            .ensure_registered_with_gh(
                "Owner/Repo",
                "https://shipyard.example/webhook",
                "secret",
                &gh,
            )
            .expect("create");
        assert_eq!(id, 4242);
        let id = registrar
            .ensure_registered_with_gh(
                "oWnEr/rEpO",
                "https://shipyard.example/webhook",
                "secret",
                &gh,
            )
            .expect("reuse");
        assert_eq!(id, 4242);
        registrar
            .unregister_with_gh("OWNER/REPO", &gh)
            .expect("unregister alias");
        assert!(registrar.all().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn delete_404_is_treated_as_success() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::Delete404);
        let mut registrar = stub_registrar(temp.path());
        registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect("create");

        registrar
            .unregister_with_gh("owner/repo", &gh)
            .expect("delete");

        assert!(registrar.all().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn create_requires_parseable_hook_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::MissingId);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect_err("missing hook id");

        assert!(matches!(error, RegistrarError::MissingHookId(_)));
        assert!(registrar.all().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn missing_local_provenance_adopts_one_exact_remote_hook_transactionally() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::AdoptExisting);
        let mut registrar = stub_registrar(temp.path());

        let hook_id = registrar
            .ensure_registered_with_gh(
                "owner/repo",
                "https://example.test/webhook",
                "rotated-secret",
                &gh,
            )
            .expect("adopt exact hook");

        assert_eq!(hook_id, 7331);
        assert_eq!(registrar.all().get("owner/repo"), Some(&7331));
        assert!(read_log(temp.path(), "args-2").contains("-X PATCH"));
        assert!(read_log(temp.path(), "args-2").contains("hooks/7331"));
        let update_body = read_json_log(temp.path(), "stdin-2");
        assert_eq!(update_body["events"], serde_json::json!(SUBSCRIBED_EVENTS));
        assert!(
            !temp.path().join("args-3").exists(),
            "must not POST a duplicate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn exact_url_adoption_rejects_partial_patch_response_without_local_commit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::AdoptPatchIncomplete);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh(
                "owner/repo",
                "https://example.test/webhook",
                "rotated-secret",
                &gh,
            )
            .expect_err("partial subscription response must fail closed");

        assert!(matches!(
            error,
            RegistrarError::HookReconciliationMismatch { hook_id: 7331, .. }
        ));
        let update_body = read_json_log(temp.path(), "stdin-2");
        assert_eq!(update_body["events"], serde_json::json!(SUBSCRIBED_EVENTS));
        assert!(registrar.all().is_empty());
        assert!(!temp.path().join("daemon/registrations.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn missing_local_provenance_does_not_persist_when_exact_hook_update_fails() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::AdoptPatchFails);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh(
                "owner/repo",
                "https://example.test/webhook",
                "rotated-secret",
                &gh,
            )
            .expect_err("failed exact-hook reconciliation");

        assert!(matches!(
            error,
            RegistrarError::GhFailed {
                action: "patch",
                ..
            }
        ));
        assert!(registrar.all().is_empty());
        assert!(!temp.path().join("daemon/registrations.json").exists());
        assert!(
            !temp.path().join("args-3").exists(),
            "must not create a duplicate"
        );
    }

    #[cfg(unix)]
    #[test]
    fn missing_local_provenance_ignores_non_exact_remote_hook_url() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::WrongUrl);
        let mut registrar = stub_registrar(temp.path());

        let hook_id = registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect("create distinct exact hook");

        assert_eq!(hook_id, 4242);
        assert!(read_log(temp.path(), "args-2").contains("-X POST"));
    }

    #[cfg(unix)]
    #[test]
    fn missing_local_provenance_fails_closed_for_ambiguous_exact_hooks() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::Ambiguous);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect_err("ambiguous remote provenance");

        assert!(
            matches!(
                error,
                RegistrarError::AmbiguousRemoteHooks { ref hook_ids, .. }
                    if hook_ids == &vec![7331, 7332]
            ),
            "unexpected error: {error:?}"
        );
        assert!(registrar.all().is_empty());
        assert!(
            !temp.path().join("args-2").exists(),
            "must not mutate remote hooks"
        );
        assert!(!temp.path().join("daemon/registrations.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn create_missing_webhook_scope_is_actionable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::MissingWebhookScope);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect_err("missing scope");

        assert!(error.is_missing_webhook_scope());
        assert!(error.to_string().contains(WEBHOOK_SCOPE_COMMAND));
        assert!(registrar.all().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn create_http_401_is_auth_degraded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::Unauthorized);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect_err("unauthorized");

        assert!(
            error.is_auth_degraded(),
            "401 should be auth-degraded, got {error:?}"
        );
        assert!(!error.is_missing_webhook_scope());
        assert!(
            error
                .auth_degraded_detail()
                .to_lowercase()
                .contains("bad credentials")
        );
        assert!(registrar.all().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn create_anonymous_rate_limit_is_auth_degraded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::AnonRateLimit);
        let mut registrar = stub_registrar(temp.path());

        let error = registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect_err("rate limited");

        assert!(
            error.is_auth_degraded(),
            "anonymous rate limit is auth-degraded"
        );
        assert!(
            error.auth_degraded_detail().contains("anonymous 60/hr"),
            "detail should name the anonymous bucket: {}",
            error.auth_degraded_detail()
        );
    }

    #[test]
    fn classify_prefers_webhook_scope_over_auth() {
        // A repo_hook 403 must stay a scope grant, not a dead-token downgrade.
        let error = super::classify_gh_failure(
            "create",
            "HTTP 403: Resource not accessible; missing admin:repo_hook".to_owned(),
        );
        assert!(error.is_missing_webhook_scope());
        assert!(!error.is_auth_degraded());
    }

    #[test]
    fn classify_detects_401_403_and_credentials() {
        for output in [
            "HTTP 401: Bad credentials",
            "gh: 401 Unauthorized",
            "HTTP 403: Forbidden",
            "This endpoint requires authentication",
        ] {
            let error = super::classify_gh_failure("create", output.to_owned());
            assert!(
                error.is_auth_degraded(),
                "should be auth-degraded: {output}"
            );
        }
    }

    #[test]
    fn classify_detects_anonymous_rate_limit_json_body() {
        // Some gh versions echo a rate_limit JSON body; core limit 60 = anon.
        let body = r#"{"resources":{"core":{"limit":60,"remaining":0}}}"#;
        let error = super::classify_gh_failure("create", body.to_owned());
        assert!(error.is_auth_degraded());
        assert!(error.auth_degraded_detail().contains("anonymous 60/hr"));
    }

    #[test]
    fn classify_leaves_generic_failure_alone() {
        let error = super::classify_gh_failure("create", "unexpected gh failure".to_owned());
        assert!(!error.is_auth_degraded());
        assert!(!error.is_missing_webhook_scope());
        assert!(matches!(error, super::RegistrarError::GhFailed { .. }));
    }

    #[test]
    fn classify_not_found_and_transient_failures() {
        let missing = super::classify_gh_failure("list", "HTTP 404: Not Found".to_owned());
        assert!(missing.is_not_found());
        assert!(!missing.is_transient());

        for output in ["HTTP 429: rate limit", "HTTP 503: Service Unavailable", "request timed out"] {
            let error = super::classify_gh_failure("patch", output.to_owned());
            assert!(error.is_transient(), "expected transient classification: {output}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn unregister_without_gh_removes_local_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let gh = write_gh_stub(temp.path(), GhStubMode::Ok);
        let mut registrar = stub_registrar(temp.path());
        registrar
            .ensure_registered_with_gh("owner/repo", "https://example.test/webhook", "secret", &gh)
            .expect("create");
        fs::remove_file(temp.path().join("api")).expect("remove gh script");

        registrar.unregister("owner/repo").expect("unregister");

        assert!(registrar.all().is_empty());
    }

    #[cfg(unix)]
    #[derive(Clone, Copy)]
    enum GhStubMode {
        Ok,
        AdoptExisting,
        AdoptPatchFails,
        AdoptPatchIncomplete,
        Ambiguous,
        WrongUrl,
        Delete404,
        MissingId,
        MissingWebhookScope,
        Unauthorized,
        AnonRateLimit,
    }

    #[cfg(unix)]
    fn stub_registrar(temp: &Path) -> Registrar {
        Registrar::new_with_context(RuntimeMode::Shipyard, temp, temp)
    }

    #[cfg(unix)]
    fn write_gh_stub(temp: &Path, mode: GhStubMode) -> PathBuf {
        let create_response = match mode {
            GhStubMode::MissingId => "{}",
            GhStubMode::Ok
            | GhStubMode::AdoptExisting
            | GhStubMode::AdoptPatchFails
            | GhStubMode::AdoptPatchIncomplete
            | GhStubMode::Ambiguous
            | GhStubMode::WrongUrl
            | GhStubMode::Delete404
            | GhStubMode::MissingWebhookScope
            | GhStubMode::Unauthorized
            | GhStubMode::AnonRateLimit => "{\"id\":4242}",
        };
        let delete_branch = match mode {
            GhStubMode::Delete404 => {
                "  *\" -X DELETE \"*) printf '404 not found\\n' >&2; exit 1 ;;"
            }
            GhStubMode::Ok
            | GhStubMode::AdoptExisting
            | GhStubMode::AdoptPatchFails
            | GhStubMode::AdoptPatchIncomplete
            | GhStubMode::Ambiguous
            | GhStubMode::WrongUrl
            | GhStubMode::MissingId
            | GhStubMode::MissingWebhookScope
            | GhStubMode::Unauthorized
            | GhStubMode::AnonRateLimit => "  *\" -X DELETE \"*) exit 0 ;;",
        };
        let create_branch = match mode {
            GhStubMode::MissingWebhookScope => String::from(
                "  *\" -X POST \"*) printf 'missing scope: admin:repo_hook\\n' >&2; exit 1 ;;",
            ),
            GhStubMode::Unauthorized => String::from(
                "  *\" -X POST \"*) printf 'HTTP 401: Bad credentials (https://api.github.com/repos/owner/repo/hooks)\\n' >&2; exit 1 ;;",
            ),
            GhStubMode::AnonRateLimit => String::from(
                "  *\" -X POST \"*) printf 'HTTP 403: API rate limit exceeded for 203.0.113.7. (But here is the good news: Authenticated requests get a higher rate limit.)\\n' >&2; exit 1 ;;",
            ),
            GhStubMode::Ok
            | GhStubMode::AdoptExisting
            | GhStubMode::AdoptPatchFails
            | GhStubMode::AdoptPatchIncomplete
            | GhStubMode::Ambiguous
            | GhStubMode::WrongUrl
            | GhStubMode::Delete404
            | GhStubMode::MissingId => {
                format!("  *\" -X POST \"*) printf '%s\\n' '{create_response}' ;;")
            }
        };
        let list_response = match mode {
            GhStubMode::AdoptExisting
            | GhStubMode::AdoptPatchFails
            | GhStubMode::AdoptPatchIncomplete => {
                r#"[[{"id":7331,"name":"web","active":false,"events":["push"],"config":{"url":"https://example.test/webhook"}}]]"#
            }
            GhStubMode::Ambiguous => {
                r#"[[{"id":7332,"name":"web","config":{"url":"https://example.test/webhook"}},{"id":7331,"name":"web","config":{"url":"https://example.test/webhook"}}]]"#
            }
            GhStubMode::WrongUrl => {
                r#"[[{"id":7331,"name":"web","config":{"url":"https://other.test/webhook"}}]]"#
            }
            _ => "[[]]",
        };
        let script = format!(
            r#"#!/bin/sh
set -eu
LOG_DIR={log_dir}
COUNT_FILE="$LOG_DIR/counter"
COUNT="$(cat "$COUNT_FILE" 2>/dev/null || printf 0)"
COUNT="$((COUNT + 1))"
printf '%s' "$COUNT" > "$COUNT_FILE"
printf '%s\n' "$*" > "$LOG_DIR/args-$COUNT"
cat > "$LOG_DIR/stdin-$COUNT" || true
case " $* " in
  *" --paginate --slurp "*) printf '%s\n' '{list_response}' ;;
{create_branch}
  *" -X PATCH "*) {patch_branch}
{delete_branch}
  *) printf 'unexpected gh args: %s\n' "$*" >&2; exit 2 ;;
esac
"#,
            log_dir = shell_quote(temp),
            create_branch = create_branch,
            delete_branch = delete_branch,
            list_response = list_response,
            patch_branch = match mode {
                GhStubMode::AdoptPatchFails => {
                    "printf 'patch failed\\n' >&2; exit 1 ;;"
                }
                GhStubMode::AdoptPatchIncomplete => {
                    "printf '%s\\n' '{\"active\":true,\"events\":[\"push\"],\"config\":{\"url\":\"https://example.test/webhook\"}}' ;;"
                }
                _ => "cat \"$LOG_DIR/stdin-$COUNT\" ;;",
            },
        );
        // Invoke a stable system executable and let it read a closed per-test
        // script from the isolated cwd. This avoids racing Linux exec against
        // a freshly generated executable while preserving the exact gh argv.
        fs::write(temp.join("api"), script).expect("write gh stub script");
        PathBuf::from("/bin/sh")
    }

    #[cfg(unix)]
    fn shell_quote(path: &Path) -> String {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }

    #[cfg(unix)]
    fn read_log(temp: &Path, name: &str) -> String {
        fs::read_to_string(temp.join(name)).expect(name)
    }

    #[cfg(unix)]
    fn read_json_log(temp: &Path, name: &str) -> Value {
        serde_json::from_str(&read_log(temp, name)).expect(name)
    }
}
