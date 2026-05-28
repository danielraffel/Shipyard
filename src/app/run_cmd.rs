use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};

use serde::Serialize;
use serde_json::Value;

use super::{CliFailure, controller_cmd::remote_enqueue_command};
use crate::config::LoadedConfig;
use crate::evidence::EvidenceStore;
use crate::executor::dispatch::{
    ExecutorDispatcher, ResolvedBackend, ResolvedTarget, ResolvedValidation, resolve_targets,
};
use crate::job::{Job, JobKind, Priority, ValidationMode};
use crate::output::write_json_envelope;
use crate::paths::RuntimePaths;
use crate::preflight::{
    EXIT_BACKEND_UNREACHABLE, ShipPreflightError, ShipPreflightOptions,
    collect_ship_preflight_with_options,
};
use crate::prepared_state::PreparedStateStore;
use crate::queue::Queue;
use crate::queue_request::QueuedExecutionEnvelope;
use crate::ship::{RunExecutionRequest, RunStores, drain_or_wait_run, submit_run};
use crate::warm_pool::{WarmPool, default_pool_path};

pub(super) struct RunCommandArgs {
    pub(super) targets: Option<String>,
    pub(super) mode: ValidationMode,
    pub(super) fail_fast: FailFastMode,
    pub(super) resume_from: Option<String>,
    pub(super) root_mismatch: RootMismatchPolicy,
    pub(super) reachability: ReachabilityPolicy,
    pub(super) skip_targets: Vec<String>,
    pub(super) warm: WarmPolicy,
    pub(super) tree_drift: TreeDriftPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FailFastMode {
    Continue,
    StopOnFirstFailure,
}

impl FailFastMode {
    pub(super) fn from_flag(enabled: bool) -> Self {
        if enabled {
            Self::StopOnFirstFailure
        } else {
            Self::Continue
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RootMismatchPolicy {
    Enforce,
    Allow,
}

impl RootMismatchPolicy {
    pub(super) fn from_flag(allow: bool) -> Self {
        if allow { Self::Allow } else { Self::Enforce }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ReachabilityPolicy {
    Enforce,
    AllowUnreachable,
}

impl ReachabilityPolicy {
    pub(super) fn from_flag(allow: bool) -> Self {
        if allow {
            Self::AllowUnreachable
        } else {
            Self::Enforce
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WarmPolicy {
    Enabled,
    Disabled,
}

impl WarmPolicy {
    pub(super) fn from_no_warm_flag(no_warm: bool) -> Self {
        if no_warm {
            Self::Disabled
        } else {
            Self::Enabled
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TreeDriftPolicy {
    Enforce,
    Allow,
}

impl TreeDriftPolicy {
    pub(super) fn from_flag(allow: bool) -> Self {
        if allow { Self::Allow } else { Self::Enforce }
    }
}

pub(super) fn run_command<W: Write>(
    args: RunCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
    runtime_paths: &RuntimePaths,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let mode = args.mode;
    let resolved =
        resolve_targets(config, mode).map_err(|error| CliFailure::new(1, error.to_string()))?;
    let skipped_targets = skipped_present(&resolved, args.targets.as_deref(), &args.skip_targets)?;
    let mut targets = select_targets(resolved, args.targets.as_deref(), &args.skip_targets)?;
    if targets.is_empty() {
        return Err(CliFailure::new(
            2,
            "No targets remain after --skip-target filtering.",
        ));
    }
    if args.tree_drift == TreeDriftPolicy::Allow {
        set_allow_tree_drift(&mut targets);
    }

    let preflight_dispatcher = ExecutorDispatcher::new(None);
    let mut preflight = collect_ship_preflight_with_options(
        config,
        cwd,
        &runtime_paths.state_dir,
        &targets,
        &preflight_dispatcher,
        ShipPreflightOptions {
            allow_root_mismatch: args.root_mismatch == RootMismatchPolicy::Allow,
            allow_unreachable_targets: args.reachability == ReachabilityPolicy::AllowUnreachable,
        },
    )
    .map_err(|error| preflight_failure(&error))?;
    for skipped in &skipped_targets {
        preflight.warnings.push(format!(
            "Target '{skipped}' deliberately skipped (--skip-target)."
        ));
    }
    preflight.skipped_targets = skipped_targets;

    let branch = git_required(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = git_required(cwd, &["rev-parse", "HEAD"])?;

    if !json_mode {
        write_tree_drift_banner(stdout, args.tree_drift, &targets)?;
        for warning in &preflight.warnings {
            writeln!(stdout, "warning: {warning}")
                .map_err(|error| CliFailure::new(1, error.to_string()))?;
        }
    }

    let mut queue = Queue::new(runtime_paths.state_dir.clone())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let evidence = EvidenceStore::new(runtime_paths.state_dir.join("evidence"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let prepared = PreparedStateStore::new(runtime_paths.state_dir.join("prepared"))
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let warm_pool = WarmPool::new(default_pool_path(&runtime_paths.state_dir));
    let dispatcher =
        ExecutorDispatcher::new_with_state_dir(Some(prepared), &runtime_paths.state_dir);

    let request = RunExecutionRequest {
        branch,
        sha,
        mode,
        priority: Priority::Normal,
        warm_disabled: args.warm == WarmPolicy::Disabled,
        fail_fast: args.fail_fast == FailFastMode::StopOnFirstFailure,
        resume_from: args.resume_from,
        targets,
    };
    let job = submit_run(&request, &mut queue, cwd, &runtime_paths.state_dir)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    let outcome = drain_or_wait_run(
        &request,
        job.clone(),
        RunStores {
            queue: &mut queue,
            evidence: &evidence,
            warm_pool: &warm_pool,
            cwd,
            state_dir: &runtime_paths.state_dir,
        },
        &dispatcher,
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;

    if json_mode {
        write_json_envelope(
            stdout,
            "run",
            fields([
                ("run", outcome.job.to_json_value()),
                ("preflight", preflight.to_json_value()),
            ]),
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if outcome.job.passed() {
        writeln!(stdout, "All green.").map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Failed.").map_err(|error| CliFailure::new(1, error.to_string()))?;
    }

    Ok(run_exit_code(&outcome.job))
}

pub(super) fn remote_run_command<W: Write>(
    args: RunCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
    machine_id: &str,
    json_mode: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let request = prepare_run_request(args, config, cwd)?;
    let job = Job::create(
        request.sha.clone(),
        request.branch.clone(),
        request
            .targets
            .iter()
            .map(|target| target.name.clone())
            .collect(),
        request.mode,
        request.priority,
    )
    .with_kind(JobKind::Run);
    let enqueue = RemoteRunEnqueueRequest {
        bearer_token: configured_node_token(config)?,
        envelope: QueuedExecutionEnvelope::from_run_request(job.id, cwd, &request),
    };
    remote_enqueue_command(config, machine_id, &enqueue, json_mode, "run", stdout)
}

#[derive(Serialize)]
struct RemoteRunEnqueueRequest {
    bearer_token: String,
    envelope: QueuedExecutionEnvelope,
}

pub(super) fn configured_node_token(config: &LoadedConfig) -> Result<String, CliFailure> {
    config
        .data
        .get("multi_host")
        .and_then(toml::Value::as_table)
        .and_then(|multi| multi.get("client"))
        .and_then(toml::Value::as_table)
        .and_then(|client| client.get("node_token"))
        .and_then(toml::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| CliFailure::new(1, "multi_host.client.node_token is missing"))
}

fn prepare_run_request(
    args: RunCommandArgs,
    config: &LoadedConfig,
    cwd: &Path,
) -> Result<RunExecutionRequest, CliFailure> {
    let mode = args.mode;
    let resolved =
        resolve_targets(config, mode).map_err(|error| CliFailure::new(1, error.to_string()))?;
    skipped_present(&resolved, args.targets.as_deref(), &args.skip_targets)?;
    let mut targets = select_targets(resolved, args.targets.as_deref(), &args.skip_targets)?;
    if targets.is_empty() {
        return Err(CliFailure::new(
            2,
            "No targets remain after --skip-target filtering.",
        ));
    }
    if args.tree_drift == TreeDriftPolicy::Allow {
        set_allow_tree_drift(&mut targets);
    }

    let branch = git_required(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    let sha = git_required(cwd, &["rev-parse", "HEAD"])?;

    Ok(RunExecutionRequest {
        branch,
        sha,
        mode,
        priority: Priority::Normal,
        warm_disabled: args.warm == WarmPolicy::Disabled,
        fail_fast: args.fail_fast == FailFastMode::StopOnFirstFailure,
        resume_from: args.resume_from,
        targets,
    })
}

fn preflight_failure(error: &ShipPreflightError) -> CliFailure {
    let code = match error {
        ShipPreflightError::RootMismatch { .. } => 1,
        ShipPreflightError::BackendUnreachable { .. } => EXIT_BACKEND_UNREACHABLE,
    };
    CliFailure::new(code, error.to_string())
}

fn select_targets(
    resolved: Vec<ResolvedTarget>,
    requested: Option<&str>,
    skip_targets: &[String],
) -> Result<Vec<ResolvedTarget>, CliFailure> {
    let requested_names = requested.map(parse_target_list);
    let skip = skip_targets
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let known = resolved
        .iter()
        .map(|target| target.name.as_str())
        .collect::<BTreeSet<_>>();
    if let Some(names) = &requested_names
        && let Some(missing) = names.iter().find(|name| !known.contains(name.as_str()))
    {
        return Err(CliFailure::new(1, format!("Unknown target '{missing}'")));
    }
    Ok(resolved
        .into_iter()
        .filter(|target| {
            requested_names
                .as_ref()
                .is_none_or(|names| names.contains(&target.name))
                && !skip.contains(target.name.as_str())
        })
        .collect())
}

fn skipped_present(
    resolved: &[ResolvedTarget],
    requested: Option<&str>,
    skip_targets: &[String],
) -> Result<Vec<String>, CliFailure> {
    let known_targets = resolved
        .iter()
        .map(|target| target.name.as_str())
        .collect::<BTreeSet<_>>();
    let requested_names = requested.map(parse_target_list);
    let skip_scope = requested_names.as_ref().map_or_else(
        || known_targets.clone(),
        |names| names.iter().map(String::as_str).collect::<BTreeSet<_>>(),
    );
    let mut skipped = Vec::new();
    let mut missing = Vec::new();
    for name in skip_targets {
        if skip_scope.contains(name.as_str()) {
            skipped.push(name.clone());
        } else {
            missing.push(name.clone());
        }
    }
    if !missing.is_empty() {
        missing.sort();
        return Err(CliFailure::new(
            1,
            format!(
                "skip-target names no configured target: {}",
                missing.join(", ")
            ),
        ));
    }
    Ok(skipped)
}

fn parse_target_list(raw: &str) -> BTreeSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn write_tree_drift_banner<W: Write>(
    stdout: &mut W,
    policy: TreeDriftPolicy,
    targets: &[ResolvedTarget],
) -> Result<(), CliFailure> {
    let local_targets = targets
        .iter()
        .filter(|target| matches!(&target.backend, ResolvedBackend::Local(_)))
        .map(|target| target.name.as_str())
        .collect::<Vec<_>>();
    let message = if local_targets.is_empty() {
        "Avoid editing source files mid-run on remote targets -- drift detection only fires for local targets (#249). Use a separate worktree for parallel work.".to_owned()
    } else if policy == TreeDriftPolicy::Allow {
        "--allow-tree-drift active: working-tree drift guard suppressed for this run. Mid-run edits will NOT be caught on local targets.".to_owned()
    } else {
        let scope = if local_targets.len() == targets.len() {
            "all targets".to_owned()
        } else {
            format!("local targets ({})", local_targets.join(", "))
        };
        format!(
            "Drift guard active (#249) for {scope}: mid-run edits abort the run with TREE_DRIFT (exit 3). Remote targets (ssh / cloud) are NOT covered."
        )
    };
    writeln!(stdout, "{message}").map_err(|error| CliFailure::new(1, error.to_string()))
}

fn set_allow_tree_drift(targets: &mut [ResolvedTarget]) {
    for target in targets {
        if let ResolvedValidation::Local(validation) = &mut target.validation {
            validation.allow_tree_drift = true;
        }
    }
}

fn run_exit_code(job: &crate::job::Job) -> ExitCode {
    if job
        .results
        .values()
        .any(|result| result.failure_class.as_deref() == Some("TREE_DRIFT"))
    {
        return ExitCode::from(3);
    }
    if job.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn git_required(cwd: &Path, args: &[&str]) -> Result<String, CliFailure> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|_| CliFailure::new(1, "Not in a git repository"))?;
    if !output.status.success() {
        return Err(CliFailure::new(1, "Not in a git repository"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn fields(items: impl IntoIterator<Item = (&'static str, Value)>) -> BTreeMap<String, Value> {
    items
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::process::{Command, ExitCode, Stdio};

    use toml::Table;

    use super::{
        FailFastMode, ReachabilityPolicy, RootMismatchPolicy, RunCommandArgs, TreeDriftPolicy,
        WarmPolicy, run_command,
    };
    use crate::config::{LoadedConfig, LocalOverlaySource};
    use crate::executor::dispatch::resolve_targets;
    use crate::identity::RuntimeMode;
    use crate::job::ValidationMode;
    use crate::paths::RuntimePaths;

    fn git(args: &[&str], cwd: &std::path::Path) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git command should run");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn seed_repo(repo: &std::path::Path) {
        std::fs::create_dir_all(repo).expect("repo dir");
        git(&["init", "-q"], repo);
        git(&["checkout", "-b", "feature"], repo);
        std::fs::write(repo.join("source.txt"), "initial\n").expect("seed source");
        git(&["add", "source.txt"], repo);
        git(&["commit", "-qm", "initial"], repo);
    }

    fn loaded_config(root: &std::path::Path, repo: &std::path::Path) -> LoadedConfig {
        let repo = toml_string(repo);
        let config = format!(
            r#"
            [validation.default]
            setup = "printf changed > source.txt"
            build = "echo SHOULD_NOT_RUN"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            cwd = "{repo}"
            "#,
        )
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data: config,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn loaded_config_two_targets(root: &std::path::Path, repo: &std::path::Path) -> LoadedConfig {
        let repo = toml_string(repo);
        let config = format!(
            r#"
            [validation.default]
            command = "true"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            cwd = "{repo}"

            [targets.linux]
            backend = "local"
            platform = "linux-x64"
            cwd = "{repo}"
            "#,
        )
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data: config,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn loaded_config_mixed_targets(root: &std::path::Path, repo: &std::path::Path) -> LoadedConfig {
        let repo = toml_string(repo);
        let config = format!(
            r#"
            [validation.default]
            command = "true"

            [targets.mac]
            backend = "local"
            platform = "macos-arm64"
            cwd = "{repo}"

            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            host = "linux"
            repo_path = "~/repo"
            "#,
        )
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data: config,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn loaded_config_remote_target(root: &std::path::Path) -> LoadedConfig {
        let config = r#"
            [validation.default]
            command = "true"

            [targets.linux]
            backend = "ssh"
            platform = "linux-x64"
            host = "linux"
            repo_path = "~/repo"
            "#
        .parse::<Table>()
        .expect("config TOML");
        LoadedConfig {
            data: config,
            global_dir: root.join("global"),
            project_dir: None,
            local_dir: None,
            local_overlay_source: LocalOverlaySource::None,
        }
    }

    fn toml_string(path: &std::path::Path) -> String {
        path.display().to_string().replace('\\', "\\\\")
    }

    fn args(allow_tree_drift: bool) -> RunCommandArgs {
        RunCommandArgs {
            targets: None,
            mode: crate::job::ValidationMode::Full,
            fail_fast: FailFastMode::Continue,
            resume_from: None,
            root_mismatch: RootMismatchPolicy::Enforce,
            reachability: ReachabilityPolicy::Enforce,
            skip_targets: Vec::new(),
            warm: WarmPolicy::Disabled,
            tree_drift: TreeDriftPolicy::from_flag(allow_tree_drift),
        }
    }

    #[test]
    fn run_command_exits_three_on_tree_drift() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = run_command(
            args(false),
            &loaded_config(temp.path(), &repo),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("run command");

        assert_eq!(code, ExitCode::from(3));
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        assert_eq!(output["command"], "run");
        assert_eq!(output["run"]["overall"], "fail");
        assert_eq!(
            output["run"]["results"]["mac"]["failure_class"],
            "TREE_DRIFT"
        );
    }

    #[test]
    fn run_command_allow_tree_drift_runs_later_stage() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = run_command(
            args(true),
            &loaded_config(temp.path(), &repo),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect("run command");

        assert_eq!(code, ExitCode::SUCCESS);
        let output: serde_json::Value = serde_json::from_slice(&stdout).expect("json");
        let repo = repo.canonicalize().expect("canonical repo");
        assert_eq!(output["run"]["overall"], "pass");
        assert_eq!(output["preflight"]["git_root"], repo.display().to_string());
        assert_eq!(
            output["preflight"]["expected_root"],
            repo.display().to_string()
        );
        assert_eq!(output["preflight"]["targets"]["mac"]["target"], "mac");
        assert_eq!(output["preflight"]["targets"]["mac"]["backend"], "local");
        assert_eq!(output["preflight"]["targets"]["mac"]["reachable"], true);
        assert_eq!(
            output["preflight"]["targets"]["mac"]["selected_backend"],
            "local"
        );
        assert_eq!(
            output["preflight"]["warnings"],
            serde_json::Value::Array(Vec::new())
        );
        assert_eq!(
            output["preflight"]["skipped_targets"],
            serde_json::Value::Array(Vec::new())
        );
    }

    #[test]
    fn run_command_human_banner_reflects_tree_drift_policy() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = run_command(
            args(false),
            &loaded_config_two_targets(temp.path(), &repo),
            &repo,
            &paths,
            false,
            &mut stdout,
        )
        .expect("run command");

        assert_eq!(code, ExitCode::SUCCESS);
        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("Drift guard active (#249)"));
        assert!(text.contains("for all targets"));
        assert!(text.contains("TREE_DRIFT (exit 3)"));
        assert!(text.contains("Remote targets (ssh / cloud) are NOT covered"));
        assert!(!text.contains("--allow-tree-drift active"));

        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut stdout = Vec::new();

        let code = run_command(
            args(true),
            &loaded_config_two_targets(temp.path(), &repo),
            &repo,
            &paths,
            false,
            &mut stdout,
        )
        .expect("run command");

        assert_eq!(code, ExitCode::SUCCESS);
        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("--allow-tree-drift active"));
        assert!(text.contains("guard suppressed"));
        assert!(text.contains("NOT be caught on local targets"));
        assert!(!text.contains("Drift guard active (#249)"));
    }

    #[test]
    fn tree_drift_banner_only_promises_local_target_enforcement() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        let mixed = resolve_targets(
            &loaded_config_mixed_targets(temp.path(), &repo),
            ValidationMode::Full,
        )
        .expect("mixed targets");
        let mut stdout = Vec::new();

        super::write_tree_drift_banner(&mut stdout, TreeDriftPolicy::Enforce, &mixed)
            .expect("banner");

        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("Drift guard active (#249) for local targets (mac)"));
        assert!(text.contains("Remote targets (ssh / cloud) are NOT covered"));

        let remote = resolve_targets(
            &loaded_config_remote_target(temp.path()),
            ValidationMode::Full,
        )
        .expect("remote target");
        let mut stdout = Vec::new();

        super::write_tree_drift_banner(&mut stdout, TreeDriftPolicy::Enforce, &remote)
            .expect("banner");

        let text = String::from_utf8(stdout).expect("utf8");
        assert!(text.contains("drift detection only fires for local targets"));
        assert!(!text.contains("Drift guard active (#249)"));
    }

    #[test]
    fn run_command_rejects_unknown_skip_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut command_args = args(false);
        command_args.skip_targets = vec!["missing".to_owned()];
        let mut stdout = Vec::new();

        let error = run_command(
            command_args,
            &loaded_config(temp.path(), &repo),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect_err("unknown skip target should fail");

        assert_eq!(error.code, 1);
        assert_eq!(
            error.message,
            "skip-target names no configured target: missing"
        );
    }

    #[test]
    fn run_command_rejects_skip_target_outside_requested_set() {
        let temp = tempfile::tempdir().expect("tempdir");
        let repo = temp.path().join("repo");
        seed_repo(&repo);
        let paths = RuntimePaths::current_with_overrides(
            RuntimeMode::Isolated,
            Some(temp.path().join("global")),
            Some(temp.path().join("state")),
        );
        let mut command_args = args(false);
        command_args.targets = Some("mac".to_owned());
        command_args.skip_targets = vec!["linux".to_owned()];
        let mut stdout = Vec::new();

        let error = run_command(
            command_args,
            &loaded_config_two_targets(temp.path(), &repo),
            &repo,
            &paths,
            true,
            &mut stdout,
        )
        .expect_err("skip outside requested target set should fail");

        assert_eq!(error.code, 1);
        assert_eq!(
            error.message,
            "skip-target names no configured target: linux"
        );
    }
}
