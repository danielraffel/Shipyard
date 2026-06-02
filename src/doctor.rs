use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::Serialize;

use crate::cloud::{GitHubActions, workflow_run_is_newer};
use crate::config::LoadedConfig;
use crate::daemon_version::{DaemonVersionRelation, read_daemon_version_relation};
use crate::executor::dispatch::{
    ExecutorDispatcher, ResolvedBackend, ResolvedTarget, resolve_targets,
};
use crate::gh::{
    GhAuthPolicy, GhAuthSourceSummary, GhAuthSummary, GhClient, GhPrepareError, GhSupervision,
};
use crate::identity::RuntimeMode;
use crate::job::ValidationMode;

const RELEASE_CHAIN_WORKFLOW: &str = "auto-release.yml";
const RELEASE_CHAIN_POLL_ATTEMPTS: usize = 30;
const RELEASE_CHAIN_POLL_INTERVAL_SECS: u64 = 10;

/// A single doctor entry shown in one section of the report.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DoctorEntry {
    /// Whether the check passed.
    pub ok: bool,
    /// Short version or summary line when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Detailed explanation when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Error summary when the tool is missing or broken.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl DoctorEntry {
    fn ok(version: impl Into<String>) -> Self {
        Self {
            ok: true,
            version: Some(version.into()),
            detail: None,
            error: None,
        }
    }

    fn missing() -> Self {
        Self {
            ok: false,
            version: None,
            detail: None,
            error: Some("not installed".to_owned()),
        }
    }
}

/// Structured doctor report consumed by the CLI and GUI.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DoctorReport {
    /// Overall readiness result.
    pub ready: bool,
    /// Sectioned doctor checks.
    pub checks: BTreeMap<String, BTreeMap<String, DoctorEntry>>,
}

/// Abstraction over command probing for testability.
pub trait CommandProbe {
    /// Return a single-line version string on success.
    fn probe(&self, command: &str, args: &[&str]) -> Option<String>;

    /// Return full command output when a check needs more than one line.
    fn run(&self, command: &str, args: &[&str]) -> Option<DoctorCommandOutput> {
        self.probe(command, args).map(|stdout| DoctorCommandOutput {
            success: true,
            stdout,
            stderr: String::new(),
        })
    }
}

/// Captured output from a doctor command probe.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DoctorCommandOutput {
    /// Whether the command exited successfully.
    pub success: bool,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

/// Default probe implementation that shells out to the local machine.
pub struct SystemCommandProbe;

impl CommandProbe for SystemCommandProbe {
    fn probe(&self, command: &str, args: &[&str]) -> Option<String> {
        let output = self.run(command, args)?;
        let stdout = output.stdout;
        let stderr = output.stderr;
        let line = stdout
            .lines()
            .find(|line| !line.trim().is_empty())
            .or_else(|| stderr.lines().find(|line| !line.trim().is_empty()))?;
        Some(line.trim().to_owned())
    }

    fn run(&self, command: &str, args: &[&str]) -> Option<DoctorCommandOutput> {
        let output = Command::new(command).args(args).output().ok()?;
        Some(DoctorCommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }
}

/// Collect the current machine-scoped doctor report.
#[must_use]
pub fn collect_report(
    probe: &impl CommandProbe,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
) -> DoctorReport {
    let mut checks = BTreeMap::new();

    let mut core = BTreeMap::new();
    core.insert(
        "git".to_owned(),
        check_command(probe, "git", &["--version"]),
    );
    core.insert("ssh".to_owned(), check_command(probe, "ssh", &["-V"]));
    core.insert("rich-bundle".to_owned(), check_rich_bundle_health());
    core.insert("shipyard-on-path".to_owned(), check_shipyard_path_shadows());
    if let Some(entry) = check_macos_gatekeeper_health() {
        core.insert("macos-gatekeeper".to_owned(), entry);
    }
    if let Some(entry) =
        check_daemon_version_drift(state_dir, &format!("v{}", env!("CARGO_PKG_VERSION")))
    {
        core.insert("daemon-version".to_owned(), entry);
    }
    let ready = ["git", "ssh", "rich-bundle"]
        .iter()
        .all(|name| core.get(*name).is_some_and(|entry| entry.ok))
        && core.get("macos-gatekeeper").is_none_or(|entry| entry.ok)
        && core.get("daemon-version").is_none_or(|entry| entry.ok);
    checks.insert("Core".to_owned(), core);

    let mut cloud = BTreeMap::new();
    cloud.insert("gh".to_owned(), check_command(probe, "gh", &["--version"]));
    let github_auth = github_auth_summary(mode, cwd);
    cloud.insert(
        "github-auth".to_owned(),
        github_auth_entry_result(&github_auth),
    );
    if let Ok(summary) = &github_auth
        && let Some(entry) = check_gh_workflow_scope_for_auth(probe, summary)
    {
        cloud.insert("gh-scope".to_owned(), entry);
    }
    cloud.insert(
        "nsc".to_owned(),
        nsc_entry(
            probe.probe("nsc", &["version"]),
            namespace_in_use(mode, cwd),
        ),
    );
    checks.insert("Cloud providers".to_owned(), cloud);

    let mut release = BTreeMap::new();
    if let Some(entry) = check_release_bot_token(mode, cwd) {
        release.insert("RELEASE_BOT_TOKEN".to_owned(), entry);
    }
    if let Some(entry) = check_tag_drift(cwd, 3) {
        release.insert("tag_drift".to_owned(), entry);
    }
    if !release.is_empty() {
        checks.insert("Release pipeline".to_owned(), release);
    }

    DoctorReport { ready, checks }
}

/// Probe configured non-local runner targets using the same dispatcher
/// diagnostics used by run/ship preflight.
#[must_use]
pub fn collect_runner_checks(config: &LoadedConfig) -> Option<BTreeMap<String, DoctorEntry>> {
    let targets = match resolve_targets(config, ValidationMode::Full) {
        Ok(targets) => targets,
        Err(error) => {
            return Some(runner_config_error_checks(error.to_string()));
        }
    };
    let runner_targets: Vec<_> = targets.into_iter().filter(is_runner_target).collect();
    if runner_targets.is_empty() {
        return None;
    }

    let dispatcher = ExecutorDispatcher::new(None);
    let mut rows = BTreeMap::new();
    for target in runner_targets {
        rows.insert(target.name.clone(), runner_check(&dispatcher, &target));
    }
    Some(rows)
}

/// Return a Runners section for invalid configuration.
#[must_use]
pub fn runner_config_error_checks(detail: impl Into<String>) -> BTreeMap<String, DoctorEntry> {
    BTreeMap::from([(
        "config".to_owned(),
        DoctorEntry {
            ok: false,
            version: Some("misconfigured".to_owned()),
            detail: Some(detail.into()),
            error: None,
        },
    )])
}

fn check_command(probe: &impl CommandProbe, command: &str, args: &[&str]) -> DoctorEntry {
    probe
        .probe(command, args)
        .map_or_else(DoctorEntry::missing, DoctorEntry::ok)
}

/// Doctor entry for the Namespace CLI (`nsc`).
///
/// `nsc` is only needed when CI runs on Namespace runners. Treat it as an
/// optional provider: if it's installed, report its version; if it's absent and
/// the user hasn't configured Namespace anywhere, report a calm "not configured
/// (optional)" green row (same idiom as `rich-bundle`) rather than a red ✗.
/// It only fails when Namespace IS configured but the CLI is missing.
fn nsc_entry(version: Option<String>, namespace_configured: bool) -> DoctorEntry {
    match version {
        Some(version) => DoctorEntry::ok(version),
        None if namespace_configured => DoctorEntry::missing(),
        None => DoctorEntry {
            ok: true,
            version: Some("not configured (optional)".to_owned()),
            detail: Some(
                "Namespace Cloud CLI (nsc) is not installed. It's only needed if you run CI on Namespace runners; setups using GitHub-hosted or local runners can ignore this.".to_owned(),
            ),
            error: None,
        },
    }
}

/// Whether the merged config opts into Namespace anywhere — either the global
/// `cloud.provider = "namespace"` or a per-target provider override. Used to
/// decide whether a missing `nsc` is a real failure or just an unused optional.
/// Returns false (treat as not in use) when config can't be loaded.
fn namespace_in_use(mode: RuntimeMode, cwd: &Path) -> bool {
    let Ok(config) = LoadedConfig::load_from_cwd(mode, cwd) else {
        return false;
    };
    if config.get_str("cloud.provider") == Some("namespace") {
        return true;
    }
    let Some(targets) = config.data.get("targets").and_then(toml::Value::as_table) else {
        return false;
    };
    targets.values().any(|target| {
        target.as_table().is_some_and(|table| {
            ["provider", "runner_provider"]
                .iter()
                .any(|key| table.get(*key).and_then(toml::Value::as_str) == Some("namespace"))
        })
    })
}

fn is_runner_target(target: &ResolvedTarget) -> bool {
    !matches!(target.backend, ResolvedBackend::Local(_))
}

fn runner_check(dispatcher: &ExecutorDispatcher, target: &ResolvedTarget) -> DoctorEntry {
    if matches!(
        target.backend,
        ResolvedBackend::Ssh(_) | ResolvedBackend::Windows(_)
    ) && target.host.as_deref().is_none_or(str::is_empty)
    {
        return DoctorEntry {
            ok: false,
            version: Some("misconfigured".to_owned()),
            detail: Some("target has no `host` field".to_owned()),
            error: None,
        };
    }

    let diagnostic = dispatcher.diagnose(target);
    if diagnostic.reachable {
        return DoctorEntry {
            ok: true,
            version: Some(format!("reachable ({})", runner_label(target))),
            detail: None,
            error: None,
        };
    }

    let mut detail = format!(
        "{}: {}",
        runner_label(target),
        diagnostic.message.as_deref().unwrap_or("unreachable")
    );
    if let Some(category) = diagnostic.category.as_deref() {
        let _ = write!(detail, " [category={category}]");
    }
    DoctorEntry {
        ok: false,
        version: Some("unreachable".to_owned()),
        detail: Some(detail),
        error: None,
    }
}

fn runner_label(target: &ResolvedTarget) -> String {
    target
        .host
        .as_deref()
        .filter(|host| !host.is_empty())
        .unwrap_or(&target.backend_name)
        .to_owned()
}

fn check_rich_bundle_health() -> DoctorEntry {
    DoctorEntry {
        ok: true,
        version: Some("not applicable (native renderer)".to_owned()),
        detail: Some(
            "Shipyard no longer embeds Python rich/_unicode_data; this row is retained so doctor JSON keeps the upstream diagnostic surface.".to_owned(),
        ),
        error: None,
    }
}

fn check_shipyard_path_shadows() -> DoctorEntry {
    let path_env = std::env::var_os("PATH").unwrap_or_default();
    check_shipyard_path_shadows_with(
        &path_env.to_string_lossy(),
        &format!("v{}", env!("CARGO_PKG_VERSION")),
        |binary| {
            let output = Command::new(binary).arg("--version").output().ok()?;
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            Some(
                stdout
                    .lines()
                    .chain(stderr.lines())
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or_default()
                    .trim()
                    .to_owned(),
            )
        },
    )
}

fn check_macos_gatekeeper_health() -> Option<DoctorEntry> {
    if !cfg!(target_os = "macos") || cfg!(debug_assertions) {
        return None;
    }
    let binary = std::env::current_exe().ok()?;
    check_macos_gatekeeper_health_with(true, &binary, |command, args| {
        let output = Command::new(command).args(args).output().ok()?;
        Some(DoctorCommandOutput {
            success: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    })
}

fn check_macos_gatekeeper_health_with<F>(
    enabled: bool,
    binary: &Path,
    mut run_command: F,
) -> Option<DoctorEntry>
where
    F: FnMut(&str, &[String]) -> Option<DoctorCommandOutput>,
{
    if !enabled || !binary.exists() {
        return None;
    }

    let binary_arg = binary.display().to_string();
    let mut problems = Vec::new();

    if let Some(output) = run_command("xattr", std::slice::from_ref(&binary_arg))
        && output.stdout.contains("com.apple.quarantine")
    {
        problems.push(format!(
            "com.apple.quarantine xattr present — binary is in Gatekeeper first-run evaluation state. Clear with: xattr -d com.apple.quarantine {binary_arg}"
        ));
    }

    if let Some(output) = run_command(
        "codesign",
        &[
            "--verify".to_owned(),
            "--deep".to_owned(),
            binary_arg.clone(),
        ],
    ) && !output.success
    {
        let detail = first_non_empty(&output.stderr, &output.stdout)
            .unwrap_or("verification failed")
            .chars()
            .take(200)
            .collect::<String>();
        problems.push(format!(
            "codesign --verify failed: {detail}. Binary signature is broken; reinstall from the release artifact via install.sh."
        ));
    }

    if problems.is_empty() {
        return Some(DoctorEntry::ok("codesign + xattr healthy"));
    }

    Some(DoctorEntry {
        ok: false,
        version: Some("macOS signing integrity issue".to_owned()),
        detail: Some(problems.join("\n  ")),
        error: None,
    })
}

fn first_non_empty<'a>(first: &'a str, second: &'a str) -> Option<&'a str> {
    [first.trim(), second.trim()]
        .into_iter()
        .find(|value| !value.is_empty())
}

fn check_shipyard_path_shadows_with<F>(
    path_env: &str,
    self_version: &str,
    mut version_for_binary: F,
) -> DoctorEntry
where
    F: FnMut(&Path) -> Option<String>,
{
    let binaries = enumerate_path_binaries("shipyard", path_env);
    if binaries.is_empty() {
        return DoctorEntry {
            ok: true,
            version: Some(format!("{self_version} (shipyard not on PATH)")),
            detail: None,
            error: None,
        };
    }
    if binaries.len() == 1 {
        return DoctorEntry {
            ok: true,
            version: Some(format!("{self_version} (single binary on PATH)")),
            detail: None,
            error: None,
        };
    }

    let winner = &binaries[0];
    let rows = binaries
        .iter()
        .enumerate()
        .map(|(index, binary)| {
            let label = if index == 0 {
                "WINS".to_owned()
            } else {
                format!("shadowed by {}", winner.display())
            };
            let version = version_for_binary(binary).unwrap_or_default();
            format!("    {} -> {}  [{}]", binary.display(), version, label)
        })
        .collect::<Vec<_>>();
    let detail = format!(
        "Multiple `shipyard` binaries on PATH — if PATH order ever shifts, a stale one could silently shadow the active one.\n{}\n  Fix: remove the unused entries (e.g. {}) or reorder PATH so the intended binary stays first.",
        rows.join("\n"),
        binaries
            .last()
            .map_or_else(String::new, |path| path.display().to_string())
    );

    DoctorEntry {
        ok: false,
        version: Some(format!(
            "{self_version} ({} binaries found)",
            binaries.len()
        )),
        detail: Some(detail),
        error: None,
    }
}

fn enumerate_path_binaries(binary_name: &str, path_env: &str) -> Vec<PathBuf> {
    let mut binaries = Vec::new();
    let mut seen_real = std::collections::BTreeSet::new();
    let candidate_names = if cfg!(windows) {
        vec![format!("{binary_name}.exe"), binary_name.to_owned()]
    } else {
        vec![binary_name.to_owned()]
    };

    for directory in std::env::split_paths(path_env) {
        for candidate_name in &candidate_names {
            let candidate = directory.join(candidate_name);
            if !candidate.is_file() {
                continue;
            }
            let real = candidate
                .canonicalize()
                .unwrap_or_else(|_| candidate.clone());
            if seen_real.insert(real) {
                binaries.push(candidate);
            }
            break;
        }
    }

    binaries
}

fn check_release_bot_token(mode: RuntimeMode, cwd: &Path) -> Option<DoctorEntry> {
    let repo = detect_repo_slug(cwd)?;
    check_release_bot_token_with(Some(&repo), |repo_slug| {
        let output = gh_command(mode, cwd)?
            .args([
                "api",
                &format!("repos/{repo_slug}/actions/secrets"),
                "--paginate",
                "--jq",
                ".secrets[].name",
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    })
}

/// Dispatch auto-release.yml and report whether the release-bot checkout chain works.
#[must_use]
pub fn check_release_chain(cwd: &Path) -> Option<DoctorEntry> {
    check_release_chain_with_mode(RuntimeMode::Shipyard, cwd)
}

/// Dispatch auto-release.yml using the configured GitHub auth for `mode`.
#[must_use]
pub fn check_release_chain_with_mode(mode: RuntimeMode, cwd: &Path) -> Option<DoctorEntry> {
    let repo = detect_repo_slug(cwd)?;
    Some(check_release_chain_for_repo(
        mode,
        cwd,
        &repo,
        release_bot_secret_state(mode, cwd),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReleaseBotSecretState {
    Present,
    Missing,
    Unknown,
}

fn release_bot_secret_state(mode: RuntimeMode, cwd: &Path) -> ReleaseBotSecretState {
    match check_release_bot_token(mode, cwd) {
        Some(entry) if entry.ok => ReleaseBotSecretState::Present,
        Some(_) => ReleaseBotSecretState::Missing,
        None => ReleaseBotSecretState::Unknown,
    }
}

fn check_release_chain_for_repo(
    mode: RuntimeMode,
    cwd: &Path,
    repo: &str,
    secret_state: ReleaseBotSecretState,
) -> DoctorEntry {
    let branch = default_branch(mode, cwd, repo).unwrap_or_else(|| "main".to_owned());
    let client = GitHubActions::from_cwd(mode, cwd);
    let baseline = match client.latest_workflow_run_for_branch(
        Some(repo),
        RELEASE_CHAIN_WORKFLOW,
        &branch,
    ) {
        Ok(run) => run.map(|run| run.database_id),
        Err(error) => {
            return release_chain_entry(
                false,
                "dispatch-failed",
                format!(
                    "Couldn't establish a run-ID baseline before dispatch. {error}. Retry once gh is reachable."
                ),
            );
        }
    };

    if let Err(error) = client.workflow_dispatch(
        Some(repo),
        RELEASE_CHAIN_WORKFLOW,
        &branch,
        &BTreeMap::new(),
    ) {
        return release_chain_entry(
            false,
            "dispatch-failed",
            format!(
                "gh workflow run failed. {error}. The workflow may not accept workflow_dispatch."
            ),
        );
    }

    let mut last_error = None;
    for _ in 0..RELEASE_CHAIN_POLL_ATTEMPTS {
        match client.latest_workflow_run_for_branch(Some(repo), RELEASE_CHAIN_WORKFLOW, &branch) {
            Ok(Some(run)) if run.status == "completed" && workflow_run_is_newer(&run, baseline) => {
                return release_chain_result_entry(
                    run.conclusion.as_deref().unwrap_or("unknown"),
                    secret_state,
                );
            }
            Ok(_) => {}
            Err(error) => last_error = Some(error.to_string()),
        }
        std::thread::sleep(std::time::Duration::from_secs(
            RELEASE_CHAIN_POLL_INTERVAL_SECS,
        ));
    }

    let detail = last_error.map_or_else(
        || "Verification workflow didn't complete in 5 min. Check Actions tab manually.".to_owned(),
        |error| {
            format!("Verification workflow didn't complete in 5 min. Last polling error: {error}")
        },
    );
    release_chain_entry(false, "timeout", detail)
}

fn release_chain_result_entry(
    conclusion: &str,
    secret_state: ReleaseBotSecretState,
) -> DoctorEntry {
    if conclusion == "success" {
        return match secret_state {
            ReleaseBotSecretState::Present => release_chain_entry(
                true,
                "checkout-ok",
                "auto-release.yml dispatched and completed; actions/checkout accepted RELEASE_BOT_TOKEN.",
            ),
            ReleaseBotSecretState::Missing => release_chain_entry(
                false,
                "fallback-token",
                "auto-release.yml succeeded but RELEASE_BOT_TOKEN is missing; checkout used the GITHUB_TOKEN fallback. Tag pushes from that token won't trigger release.yml, so binary releases still won't ship. Set the secret via `shipyard release-bot setup`.",
            ),
            ReleaseBotSecretState::Unknown => release_chain_entry(
                false,
                "checkout-ok-unverified",
                "auto-release.yml dispatched and completed, but gh secret listing was unavailable so Shipyard could not prove whether RELEASE_BOT_TOKEN or the GITHUB_TOKEN fallback was used. Re-run with authenticated gh for a definitive verdict.",
            ),
        };
    }

    release_chain_entry(
        false,
        conclusion,
        "auto-release.yml did not conclude success. Most likely: the stored token's PAT scope excludes this repo, or the stored value drifted. Run `shipyard release-bot status` for a non-destructive diagnosis; `shipyard release-bot setup --reconfigure` to re-paste.",
    )
}

fn release_chain_entry(
    ok: bool,
    version: impl Into<String>,
    detail: impl Into<String>,
) -> DoctorEntry {
    DoctorEntry {
        ok,
        version: Some(version.into()),
        detail: Some(detail.into()),
        error: None,
    }
}

/// Summarize the effective GitHub auth Shipyard will apply to operational `gh`
/// calls.
#[must_use]
pub fn check_github_auth(mode: RuntimeMode, cwd: &Path) -> DoctorEntry {
    let result = github_auth_summary(mode, cwd);
    github_auth_entry_result(&result)
}

/// Why a configured `[github.auth]` source couldn't be summarized — kept typed
/// so the doctor entry can distinguish a context-dependent placeholder (benign)
/// from a genuinely broken source.
enum GhAuthCheckError {
    /// Couldn't even construct the gh client (config load / parse error).
    Client(String),
    /// The configured source failed to produce a token.
    Prepare(GhPrepareError),
}

impl GhAuthCheckError {
    fn message(&self) -> String {
        match self {
            Self::Client(error) => error.clone(),
            Self::Prepare(error) => error.to_string(),
        }
    }
}

fn github_auth_summary(mode: RuntimeMode, cwd: &Path) -> Result<GhAuthSummary, GhAuthCheckError> {
    let client = GhClient::from_cwd(mode, cwd)
        .map_err(|error| GhAuthCheckError::Client(error.to_string()))?;
    client
        .auth_summary(cwd, GhAuthPolicy::Default)
        .map_err(GhAuthCheckError::Prepare)
}

fn github_auth_entry_result(result: &Result<GhAuthSummary, GhAuthCheckError>) -> DoctorEntry {
    match result {
        Ok(summary) => github_auth_entry(summary),
        // A repo-placeholder token_command can't be filled in a repo-less
        // context (doctor/daemon run outside a checkout) — but it resolves fine
        // when Shipyard runs inside a repo. That's expected, not broken, so keep
        // it green with guidance rather than a red ✗.
        Err(GhAuthCheckError::Prepare(GhPrepareError::RepoSlugRequired)) => DoctorEntry {
            ok: true,
            version: Some("configured (resolves inside a repo)".to_owned()),
            detail: Some(
                "A [github.auth] token_command is configured with a repo placeholder ({repo_slug}/{repo_name}), which can't be filled in a repo-less context like `doctor` or the daemon. It resolves normally when Shipyard runs inside a repo. To make it resolve everywhere (e.g. an account-wide GitHub App), pin a repo by replacing {repo_slug} with `--repo <owner>/<name>`.".to_owned(),
            ),
            error: None,
        },
        Err(error) => DoctorEntry {
            ok: false,
            version: Some("misconfigured".to_owned()),
            detail: Some(format!(
                "A [github.auth] token source is configured but couldn't produce a token: {}. This only affects setups that opt into a custom token source — if you authenticate with the GitHub CLI, remove the [github.auth] section and Shipyard will use your ambient `gh` login.",
                error.message()
            )),
            error: Some("github auth unavailable".to_owned()),
        },
    }
}

fn github_auth_entry(summary: &GhAuthSummary) -> DoctorEntry {
    let mut details = vec![match &summary.source {
        GhAuthSourceSummary::GhCli => {
            "Resolved. Shipyard will use ambient gh auth. Classic PAT scopes can usually be inspected with `gh auth status`. If GH_TOKEN is exported in the parent environment, gh gives that token precedence over keychain auth.".to_owned()
        }
        GhAuthSourceSummary::Env { token_env } => format!(
            "Resolved. Shipyard will read {token_env} and inject it into child gh commands as GH_TOKEN. The token value is not stored by Shipyard."
        ),
        GhAuthSourceSummary::Command => {
            "Resolved. Shipyard ran the configured token_command and will inject the returned token into child gh commands as GH_TOKEN. Token helper results are cached only in memory when the helper provides an expiry or TTL.".to_owned()
        }
    }];

    if let Some(kind) = summary
        .token_kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
    {
        details.push(format!(
            "Token kind reported by helper: {}",
            safe_auth_value(kind)
        ));
        if kind == "github-app-installation" {
            details.push(
                "GitHub App installation token detected. Permissions live on the App installation, not on the invoking user's gh auth scopes.".to_owned(),
            );
        }
    }
    if let Some(expires_at) = summary.expires_at {
        details.push(format!("Token expires at {}.", expires_at.to_rfc3339()));
    }

    DoctorEntry {
        ok: true,
        version: Some(github_auth_summary_label(summary)),
        detail: Some(details.join("\n")),
        error: None,
    }
}

fn github_auth_summary_label(summary: &GhAuthSummary) -> String {
    let base = match &summary.source {
        GhAuthSourceSummary::GhCli => "gh-cli (ambient)".to_owned(),
        GhAuthSourceSummary::Env { token_env } => format!("env {token_env}"),
        GhAuthSourceSummary::Command => "command helper".to_owned(),
    };
    summary
        .token_kind
        .as_deref()
        .filter(|kind| !kind.is_empty())
        .map_or(base.clone(), |kind| {
            format!("{base} ({})", safe_auth_value(kind))
        })
}

fn safe_auth_value(value: &str) -> String {
    value.chars().take(80).collect()
}

fn check_gh_workflow_scope_for_auth(
    probe: &impl CommandProbe,
    summary: &GhAuthSummary,
) -> Option<DoctorEntry> {
    match &summary.source {
        GhAuthSourceSummary::GhCli => check_gh_workflow_scope(probe),
        GhAuthSourceSummary::Env { .. } | GhAuthSourceSummary::Command => {
            Some(configured_token_scope_entry(summary))
        }
    }
}

fn configured_token_scope_entry(summary: &GhAuthSummary) -> DoctorEntry {
    let detail = match &summary.source {
        GhAuthSourceSummary::Env { token_env } => format!(
            "Configured auth comes from {token_env} and is injected as GH_TOKEN. `gh auth status` reports the ambient login, so Shipyard does not use it as a scope check for this token. Confirm the token has Actions: Read and write when using cloud retarget/handoff."
        ),
        GhAuthSourceSummary::Command => {
            if summary.token_kind.as_deref() == Some("github-app-installation") {
                "Configured auth comes from a GitHub App installation token. `gh auth status` cannot inspect App installation permissions locally; confirm Actions: Read and write on the App installation when using cloud retarget/handoff.".to_owned()
            } else {
                "Configured auth comes from a token helper and is injected as GH_TOKEN. `gh auth status` reports the ambient login, so Shipyard does not use it as a scope check for this token. Confirm the helper-issued token has Actions: Read and write when using cloud retarget/handoff.".to_owned()
            }
        }
        GhAuthSourceSummary::GhCli => unreachable!("ambient gh auth is handled by gh auth status"),
    };
    DoctorEntry {
        ok: false,
        version: Some(
            "manual verification required - permissions not inspectable locally".to_owned(),
        ),
        detail: Some(detail),
        error: None,
    }
}

fn check_gh_workflow_scope(probe: &impl CommandProbe) -> Option<DoctorEntry> {
    check_gh_workflow_scope_with(|args| probe.run("gh", args))
}

fn check_gh_workflow_scope_with<F>(mut run_auth_status: F) -> Option<DoctorEntry>
where
    F: FnMut(&[&str]) -> Option<DoctorCommandOutput>,
{
    let output = run_auth_status(&["auth", "status", "--hostname", "github.com"])?;
    if !output.success {
        return None;
    }
    let combined = format!("{}{}", output.stdout, output.stderr).to_lowercase();
    if !combined.contains("token scopes") {
        return Some(DoctorEntry {
            ok: true,
            version: Some(
                "gh auth (fine-grained / app) - scope not inspectable locally".to_owned(),
            ),
            detail: None,
            error: None,
        });
    }

    if combined.contains("'workflow'") || combined.contains("\"workflow\"") {
        return Some(DoctorEntry {
            ok: true,
            version: Some("workflow scope present".to_owned()),
            detail: None,
            error: None,
        });
    }

    Some(DoctorEntry {
        ok: false,
        version: Some("gh token missing `workflow` scope".to_owned()),
        detail: Some(
            "`cloud retarget`/`cloud handoff run --apply` need workflow scope to cancel runs. Fix:\n  gh auth refresh -h github.com -s workflow\nFine-grained tokens + GitHub App identities: see docs/install.md First-run auth.".to_owned(),
        ),
        error: None,
    })
}

fn check_daemon_version_drift(state_dir: &Path, cli_version: &str) -> Option<DoctorEntry> {
    Some(daemon_version_entry(read_daemon_version_relation(
        state_dir,
        cli_version,
    )?))
}

#[cfg(test)]
fn check_daemon_version_drift_with(
    status: Option<&serde_json::Value>,
    cli_version: &str,
) -> Option<DoctorEntry> {
    Some(daemon_version_entry(
        crate::daemon_version::compare_daemon_version(status, cli_version)?,
    ))
}

fn daemon_version_entry(relation: DaemonVersionRelation) -> DoctorEntry {
    match relation {
        DaemonVersionRelation::Match { daemon_version, .. } => DoctorEntry {
            ok: true,
            version: Some(format!("daemon v{daemon_version} matches CLI")),
            detail: None,
            error: None,
        },
        DaemonVersionRelation::Mismatch {
            daemon_version,
            cli_version,
        } => DoctorEntry {
            ok: false,
            version: Some(format!("daemon: v{daemon_version}   cli: {cli_version}")),
            detail: Some(
                "The running daemon is an older build than the CLI on PATH. Run `shipyard daemon refresh` to replace it with a fresh daemon from the current binary.".to_owned(),
            ),
            error: None,
        },
        DaemonVersionRelation::UnknownDaemonVersion { cli_version } => DoctorEntry {
            ok: false,
            version: Some(format!("daemon: <unknown>   cli: {cli_version}")),
            detail: Some(
                "The running daemon predates version reporting. Run `shipyard daemon refresh` to replace it with a fresh daemon from the current binary.".to_owned(),
            ),
            error: None,
        },
    }
}

fn check_release_bot_token_with<F>(
    repo_slug: Option<&str>,
    mut secret_names: F,
) -> Option<DoctorEntry>
where
    F: FnMut(&str) -> Option<Vec<String>>,
{
    let repo_slug = repo_slug?;
    let secrets = secret_names(repo_slug)?;
    if secrets.iter().any(|name| name == "RELEASE_BOT_TOKEN") {
        return Some(DoctorEntry {
            ok: true,
            version: Some("configured".to_owned()),
            detail: Some(
                "auto-release.yml will use this for tag pushes; downstream release.yml fires on its own.".to_owned(),
            ),
            error: None,
        });
    }

    Some(DoctorEntry {
        ok: false,
        version: Some("missing".to_owned()),
        detail: Some(format!(
            "Auto-release will fall back to GITHUB_TOKEN; tag pushes won't trigger release.yml. Fix: github.com/{repo_slug}/settings/secrets/actions -> add repository secret RELEASE_BOT_TOKEN. See RELEASING.md for the full walkthrough."
        )),
        error: None,
    })
}

fn check_tag_drift(cwd: &Path, warn_threshold: u32) -> Option<DoctorEntry> {
    check_tag_drift_with(cwd, warn_threshold, |args, cwd| {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    })
}

fn check_tag_drift_with<F>(
    cwd: &Path,
    warn_threshold: u32,
    mut git_output: F,
) -> Option<DoctorEntry>
where
    F: FnMut(&[&str], &Path) -> Option<String>,
{
    let tag = git_output(
        &["describe", "--tags", "--abbrev=0", "--match", "v[0-9]*"],
        cwd,
    )?;
    if tag.is_empty() {
        return None;
    }
    let count = git_output(
        &["rev-list", "--count", &format!("{tag}..HEAD"), "--", "src/"],
        cwd,
    )?;
    let count = count.parse::<u32>().ok()?;

    if count == 0 {
        return Some(DoctorEntry {
            ok: true,
            version: Some(format!("up-to-date ({tag})")),
            detail: Some("No CLI-surface commits since the latest tag.".to_owned()),
            error: None,
        });
    }
    if count < warn_threshold {
        return Some(DoctorEntry {
            ok: true,
            version: Some(format!("{count} commit(s) ahead of {tag}")),
            detail: Some(format!(
                "A release would pick up these changes. Under threshold ({warn_threshold}), so advisory only."
            )),
            error: None,
        });
    }

    Some(DoctorEntry {
        ok: false,
        version: Some(format!("{count} commits ahead of {tag}")),
        detail: Some(
            "User-visible changes have accumulated without a release. Either bump before release or update the release process so CLI-facing fixes do not drift indefinitely.".to_owned(),
        ),
        error: None,
    })
}

fn detect_repo_slug(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_github_repo_slug(String::from_utf8_lossy(&output.stdout).trim())
}

fn default_branch(mode: RuntimeMode, cwd: &Path, repo_slug: &str) -> Option<String> {
    let output = gh_command(mode, cwd)?
        .args([
            "repo",
            "view",
            repo_slug,
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if branch.is_empty() {
        return None;
    }
    Some(branch)
}

fn gh_command(mode: RuntimeMode, cwd: &Path) -> Option<Command> {
    GhClient::from_cwd(mode, cwd)
        .ok()?
        .prepare_command(
            cwd,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .ok()
}

fn parse_github_repo_slug(remote: &str) -> Option<String> {
    let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    [
        "git@github.com:",
        "ssh://git@github.com/",
        "https://github.com/",
        "http://github.com/",
    ]
    .iter()
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        CommandProbe, DoctorCommandOutput, DoctorReport, GhAuthCheckError, ReleaseBotSecretState,
        check_daemon_version_drift_with, check_gh_workflow_scope_for_auth,
        check_gh_workflow_scope_with, check_macos_gatekeeper_health_with,
        check_release_bot_token_with, check_shipyard_path_shadows_with, check_tag_drift_with,
        collect_report, configured_token_scope_entry, github_auth_entry, github_auth_entry_result,
        is_runner_target, nsc_entry, parse_github_repo_slug, release_chain_result_entry,
        runner_check,
    };
    use crate::executor::dispatch::{ExecutorDispatcher, resolve_targets_from_table};
    use crate::gh::{GhAuthSourceSummary, GhAuthSummary, GhPrepareError};
    use crate::identity::RuntimeMode;
    use crate::job::ValidationMode;

    struct FakeProbe {
        values: std::collections::HashMap<String, String>,
    }

    impl CommandProbe for FakeProbe {
        fn probe(&self, command: &str, _args: &[&str]) -> Option<String> {
            self.values.get(command).cloned()
        }
    }

    fn report_with(values: &[(&str, &str)]) -> DoctorReport {
        let probe = FakeProbe {
            values: values
                .iter()
                .map(|(command, version)| ((*command).to_owned(), (*version).to_owned()))
                .collect(),
        };
        collect_report(
            &probe,
            RuntimeMode::Shipyard,
            Path::new("."),
            Path::new("."),
        )
    }

    #[test]
    fn ready_depends_on_core_tools_only() {
        let report = report_with(&[("git", "git version 2.48.0"), ("ssh", "OpenSSH_9.9")]);
        assert!(report.ready);
        assert!(report.checks["Core"]["git"].ok);
        assert!(report.checks["Core"]["ssh"].ok);
        assert!(!report.checks["Cloud providers"]["gh"].ok);
        // nsc is an optional provider: absent + unconfigured reads as a calm
        // green "not configured (optional)", not a red ✗.
        let nsc = &report.checks["Cloud providers"]["nsc"];
        assert!(nsc.ok);
        assert_eq!(nsc.version.as_deref(), Some("not configured (optional)"));
    }

    #[test]
    fn nsc_entry_reports_version_when_installed() {
        let entry = nsc_entry(Some("nsc 1.2.3".to_owned()), false);
        assert!(entry.ok);
        assert_eq!(entry.version.as_deref(), Some("nsc 1.2.3"));
    }

    #[test]
    fn nsc_entry_is_optional_green_when_absent_and_unconfigured() {
        let entry = nsc_entry(None, false);
        assert!(entry.ok);
        assert_eq!(entry.version.as_deref(), Some("not configured (optional)"));
        assert!(entry.error.is_none());
    }

    #[test]
    fn nsc_entry_fails_when_configured_but_missing() {
        let entry = nsc_entry(None, true);
        assert!(!entry.ok);
        assert_eq!(entry.error.as_deref(), Some("not installed"));
    }

    #[test]
    fn github_auth_repo_slug_in_repoless_context_is_green_not_misconfigured() {
        let entry = github_auth_entry_result(&Err(GhAuthCheckError::Prepare(
            GhPrepareError::RepoSlugRequired,
        )));
        assert!(
            entry.ok,
            "repo-context placeholder should not read as broken"
        );
        assert!(entry.error.is_none());
        assert!(
            entry
                .detail
                .as_deref()
                .unwrap()
                .contains("--repo <owner>/<name>"),
            "should suggest pinning a repo"
        );
    }

    #[test]
    fn github_auth_broken_source_is_red_with_gh_fallback_hint() {
        let entry = github_auth_entry_result(&Err(GhAuthCheckError::Prepare(
            GhPrepareError::HelperStdoutEmpty,
        )));
        assert!(!entry.ok);
        // No longer leads with the alarming "fails closed" wording; instead it
        // tells gh-only users they can just drop [github.auth].
        let detail = entry.detail.as_deref().unwrap();
        assert!(detail.contains("ambient `gh`"));
        assert!(!detail.contains("fails closed"));
    }

    #[test]
    fn missing_core_tool_makes_report_not_ready() {
        let report = report_with(&[("git", "git version 2.48.0")]);
        assert!(!report.ready);
        assert_eq!(
            report.checks["Core"]["ssh"].error.as_deref(),
            Some("not installed")
        );
    }

    #[test]
    fn report_sections_match_gui_expectations() {
        let report = report_with(&[
            ("git", "git version 2.48.0"),
            ("ssh", "OpenSSH_9.9"),
            ("gh", "gh version 2.78.0"),
            ("nsc", "0.0.0"),
        ]);
        assert!(report.checks.contains_key("Core"));
        assert!(report.checks.contains_key("Cloud providers"));
        assert!(report.checks["Core"]["rich-bundle"].ok);
        assert_eq!(
            report.checks["Core"]["rich-bundle"].version.as_deref(),
            Some("not applicable (native renderer)")
        );
        assert_eq!(
            report.checks["Cloud providers"]["gh"].version.as_deref(),
            Some("gh version 2.78.0")
        );
        assert!(report.checks["Cloud providers"].contains_key("gh-scope"));
    }

    #[test]
    fn runner_check_reports_missing_ssh_host_as_misconfigured() {
        let config = r#"
            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            repo_path = "/srv/repo"
        "#
        .parse()
        .expect("toml");
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0);
        let entry = runner_check(&ExecutorDispatcher::new(None), &target);

        assert!(!entry.ok);
        assert_eq!(entry.version.as_deref(), Some("misconfigured"));
        assert_eq!(entry.detail.as_deref(), Some("target has no `host` field"));
    }

    #[test]
    fn runner_filter_omits_local_targets() {
        let config = r#"
            [targets.local]
            backend = "local"
            platform = "linux-x64"
        "#
        .parse()
        .expect("toml");
        let target = resolve_targets_from_table(&config, ValidationMode::Full)
            .expect("targets")
            .remove(0);

        assert!(!is_runner_target(&target));
    }

    #[test]
    fn release_chain_success_with_missing_secret_reports_fallback_token() {
        let entry = release_chain_result_entry("success", ReleaseBotSecretState::Missing);

        assert!(!entry.ok);
        assert_eq!(entry.version.as_deref(), Some("fallback-token"));
        assert!(
            entry
                .detail
                .as_deref()
                .expect("detail")
                .contains("GITHUB_TOKEN fallback")
        );
    }

    #[test]
    fn gh_workflow_scope_uses_github_com_and_detects_classic_scope() {
        let mut requested_args = Vec::new();
        let entry = check_gh_workflow_scope_with(|args| {
            requested_args = args.iter().map(ToString::to_string).collect();
            Some(DoctorCommandOutput {
                success: true,
                stdout: String::new(),
                stderr: "Token scopes: 'gist', 'repo', 'workflow'\n".to_owned(),
            })
        })
        .expect("entry");

        assert_eq!(
            requested_args,
            ["auth", "status", "--hostname", "github.com"]
        );
        assert!(entry.ok);
        assert_eq!(entry.version.as_deref(), Some("workflow scope present"));
    }

    #[test]
    fn gh_workflow_scope_flags_missing_classic_scope() {
        let entry = check_gh_workflow_scope_with(|_| {
            Some(DoctorCommandOutput {
                success: true,
                stdout: "Token scopes: 'repo'\n".to_owned(),
                stderr: String::new(),
            })
        })
        .expect("entry");

        assert!(!entry.ok);
        assert_eq!(
            entry.version.as_deref(),
            Some("gh token missing `workflow` scope")
        );
        assert!(
            entry
                .detail
                .as_deref()
                .expect("detail")
                .contains("gh auth refresh -h github.com -s workflow")
        );
    }

    #[test]
    fn gh_workflow_scope_passes_when_scope_is_not_locally_inspectable() {
        let entry = check_gh_workflow_scope_with(|_| {
            Some(DoctorCommandOutput {
                success: true,
                stdout: "Logged in to github.com account user\n".to_owned(),
                stderr: String::new(),
            })
        })
        .expect("entry");

        assert!(entry.ok);
        assert!(
            entry
                .version
                .as_deref()
                .expect("version")
                .contains("scope not inspectable locally")
        );
    }

    #[test]
    fn gh_workflow_scope_is_silent_when_gh_auth_status_fails() {
        assert!(
            check_gh_workflow_scope_with(|_| Some(DoctorCommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "not logged in".to_owned(),
            }))
            .is_none()
        );
    }

    #[test]
    fn github_auth_entry_describes_env_source_without_token_value() {
        let summary = GhAuthSummary {
            source: GhAuthSourceSummary::Env {
                token_env: "SHIPYARD_GITHUB_TOKEN".to_owned(),
            },
            token_kind: None,
            expires_at: None,
        };

        let entry = github_auth_entry(&summary);

        assert!(entry.ok);
        assert_eq!(entry.version.as_deref(), Some("env SHIPYARD_GITHUB_TOKEN"));
        let detail = entry.detail.as_deref().expect("detail");
        assert!(detail.contains("SHIPYARD_GITHUB_TOKEN"));
        assert!(detail.contains("GH_TOKEN"));
    }

    #[test]
    fn github_auth_entry_describes_app_helper_expiry() {
        let expires_at = chrono::DateTime::parse_from_rfc3339("2026-05-26T20:12:00Z")
            .expect("time")
            .with_timezone(&chrono::Utc);
        let summary = GhAuthSummary {
            source: GhAuthSourceSummary::Command,
            token_kind: Some("github-app-installation".to_owned()),
            expires_at: Some(expires_at),
        };

        let entry = github_auth_entry(&summary);

        assert!(entry.ok);
        assert_eq!(
            entry.version.as_deref(),
            Some("command helper (github-app-installation)")
        );
        let detail = entry.detail.as_deref().expect("detail");
        assert!(detail.contains("GitHub App installation token detected"));
        assert!(detail.contains("2026-05-26T20:12:00+00:00"));
    }

    #[test]
    fn configured_token_scope_entry_marks_permissions_uninspectable() {
        let summary = GhAuthSummary {
            source: GhAuthSourceSummary::Command,
            token_kind: Some("github-app-installation".to_owned()),
            expires_at: None,
        };

        let entry = configured_token_scope_entry(&summary);

        assert!(!entry.ok);
        assert!(
            entry
                .version
                .as_deref()
                .expect("version")
                .contains("not inspectable locally")
        );
        assert!(
            entry
                .detail
                .as_deref()
                .expect("detail")
                .contains("App installation permissions")
        );
    }

    #[test]
    fn workflow_scope_uses_ambient_probe_only_for_gh_cli_auth() {
        let summary = GhAuthSummary {
            source: GhAuthSourceSummary::GhCli,
            token_kind: None,
            expires_at: None,
        };
        let probe = FakeProbe {
            values: std::collections::HashMap::from([(
                "gh".to_owned(),
                "Token scopes: 'workflow'\n".to_owned(),
            )]),
        };

        let entry = check_gh_workflow_scope_for_auth(&probe, &summary).expect("entry");

        assert!(entry.ok);
    }

    #[test]
    fn path_shadow_check_flags_multiple_binaries() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first_dir = temp.path().join("first");
        let second_dir = temp.path().join("second");
        std::fs::create_dir_all(&first_dir).expect("dir");
        std::fs::create_dir_all(&second_dir).expect("dir");
        let first = first_dir.join("shipyard");
        let second = second_dir.join("shipyard");
        std::fs::write(&first, "#!/bin/sh\nprintf '%s\\n' 'shipyard 0.21.1'\n").expect("write");
        std::fs::write(&second, "#!/bin/sh\nprintf '%s\\n' 'shipyard 0.11.0'\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
            std::fs::set_permissions(&second, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }

        let entry = check_shipyard_path_shadows_with(
            &std::env::join_paths([&first_dir, &second_dir])
                .expect("path")
                .to_string_lossy(),
            "v0.1.0",
            |binary| Some(binary.display().to_string()),
        );

        assert!(!entry.ok);
        let detail = entry.detail.expect("detail");
        assert!(detail.contains(first.to_str().expect("first")));
        assert!(detail.contains(second.to_str().expect("second")));
        assert!(detail.contains("shadowed by"));
    }

    #[test]
    fn macos_gatekeeper_check_skips_when_disabled() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        std::fs::write(&binary, "stub").expect("binary");

        assert!(
            check_macos_gatekeeper_health_with(false, &binary, |_, _| {
                panic!("disabled check must not shell out")
            })
            .is_none()
        );
    }

    #[test]
    fn macos_gatekeeper_check_reports_green() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        std::fs::write(&binary, "stub").expect("binary");

        let entry = check_macos_gatekeeper_health_with(true, &binary, |_, _| {
            Some(DoctorCommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            })
        })
        .expect("entry");

        assert!(entry.ok);
        assert_eq!(entry.version.as_deref(), Some("codesign + xattr healthy"));
    }

    #[test]
    fn macos_gatekeeper_check_surfaces_quarantine_and_codesign_without_spctl() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        std::fs::write(&binary, "stub").expect("binary");

        let entry = check_macos_gatekeeper_health_with(true, &binary, |command, _| match command {
            "xattr" => Some(DoctorCommandOutput {
                success: true,
                stdout: "com.apple.quarantine\n".to_owned(),
                stderr: String::new(),
            }),
            "spctl" => panic!("spctl --assess false-positives on bare CLI binaries"),
            "codesign" => Some(DoctorCommandOutput {
                success: false,
                stdout: String::new(),
                stderr: "code object is not signed at all".to_owned(),
            }),
            _ => None,
        })
        .expect("entry");

        assert!(!entry.ok);
        assert_eq!(
            entry.version.as_deref(),
            Some("macOS signing integrity issue")
        );
        let detail = entry.detail.expect("detail");
        assert!(detail.contains("quarantine"));
        assert!(!detail.contains("spctl"));
        assert!(detail.contains("codesign --verify failed"));
    }

    #[test]
    fn macos_gatekeeper_check_tolerates_missing_probe_tools() {
        let temp = tempfile::tempdir().expect("tempdir");
        let binary = temp.path().join("shipyard");
        std::fs::write(&binary, "stub").expect("binary");

        let entry = check_macos_gatekeeper_health_with(true, &binary, |_, _| None).expect("entry");

        assert!(entry.ok);
    }

    #[test]
    fn release_bot_token_check_reports_configured_or_missing() {
        let configured = check_release_bot_token_with(Some("owner/repo"), |_| {
            Some(vec!["RELEASE_BOT_TOKEN".to_owned(), "OTHER".to_owned()])
        })
        .expect("entry");
        assert!(configured.ok);
        assert_eq!(configured.version.as_deref(), Some("configured"));

        let missing =
            check_release_bot_token_with(Some("owner/repo"), |_| Some(vec![])).expect("entry");
        assert!(!missing.ok);
        assert_eq!(missing.version.as_deref(), Some("missing"));
        assert!(
            missing
                .detail
                .as_deref()
                .expect("detail")
                .contains("RELEASE_BOT_TOKEN")
        );
    }

    #[test]
    fn tag_drift_reports_zero_advisory_and_threshold_crossing() {
        let up_to_date = check_tag_drift_with(Path::new("."), 3, |args, _| {
            if args[0] == "describe" {
                Some("v0.8.0".to_owned())
            } else {
                Some("0".to_owned())
            }
        })
        .expect("entry");
        assert!(up_to_date.ok);
        assert!(
            up_to_date
                .version
                .as_deref()
                .expect("version")
                .contains("up-to-date")
        );

        let advisory = check_tag_drift_with(Path::new("."), 3, |args, _| {
            if args[0] == "describe" {
                Some("v0.8.0".to_owned())
            } else {
                Some("1".to_owned())
            }
        })
        .expect("entry");
        assert!(advisory.ok);
        assert!(
            advisory
                .version
                .as_deref()
                .expect("version")
                .contains("1 commit(s) ahead")
        );

        let drift = check_tag_drift_with(Path::new("."), 3, |args, _| {
            if args[0] == "describe" {
                Some("v0.8.0".to_owned())
            } else {
                Some("5".to_owned())
            }
        })
        .expect("entry");
        assert!(!drift.ok);
        assert!(
            drift
                .version
                .as_deref()
                .expect("version")
                .contains("5 commits ahead")
        );
    }

    #[test]
    fn parse_repo_slug_handles_https_and_ssh() {
        assert_eq!(
            parse_github_repo_slug("https://github.com/danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(
            parse_github_repo_slug("git@github.com:danielraffel/Shipyard.git"),
            Some("danielraffel/Shipyard".to_owned())
        );
        assert_eq!(parse_github_repo_slug("file:///tmp/repo"), None);
    }

    #[test]
    fn daemon_version_drift_reports_match_skew_and_unknown() {
        let matched = check_daemon_version_drift_with(
            Some(&serde_json::json!({"shipyard_version": "0.1.0"})),
            "v0.1.0",
        )
        .expect("entry");
        assert!(matched.ok);
        assert_eq!(
            matched.version.as_deref(),
            Some("daemon v0.1.0 matches CLI")
        );

        let skew = check_daemon_version_drift_with(
            Some(&serde_json::json!({"shipyard_version": "0.0.9"})),
            "v0.1.0",
        )
        .expect("entry");
        assert!(!skew.ok);
        assert!(
            skew.version
                .as_deref()
                .expect("version")
                .contains("daemon: v0.0.9   cli: v0.1.0")
        );
        assert!(
            skew.detail
                .as_deref()
                .expect("detail")
                .contains("shipyard daemon refresh")
        );

        let unknown =
            check_daemon_version_drift_with(Some(&serde_json::json!({})), "v0.1.0").expect("entry");
        assert!(!unknown.ok);
        assert!(
            unknown
                .version
                .as_deref()
                .expect("version")
                .contains("daemon: <unknown>   cli: v0.1.0")
        );
        assert!(
            unknown
                .detail
                .as_deref()
                .expect("detail")
                .contains("shipyard daemon refresh")
        );
    }
}
