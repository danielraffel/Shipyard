//! CLI handlers for `shipyard runner register|list|remove|tag`.
//!
//! This is the shell-out side of runner provisioning: it talks to `gh`
//! (registration/removal tokens, the runners API, the runner release asset),
//! the GitHub Actions runner's own `config.sh`/`svc.sh`, and the local
//! `~/actions-runner-*` directories. All naming/index/label/table logic is the
//! pure code in [`crate::runner_provision`]; this module only does I/O.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::CliFailure;
use super::runner_cmd::{parse_github_repo_slug, resolve_repo_slug};
use crate::cloud::GitHubActions;
use crate::output::write_json_envelope;
use crate::runner_provision::{
    ApiRunner, AuditFinding, PoolRow, audit_runners, default_labels, format_audit_table,
    format_pool_table, next_index, orphan_local_runners, pool_rows, runner_name, short_repo,
    validate_machine_tag,
};

/// Fetch every self-hosted runner for a repo across **all** pages. GitHub caps
/// `per_page` at 100, so a one-page fetch silently misses runners on a large
/// fleet — `gh api --paginate` follows the `Link` headers and `--jq '.runners[]'`
/// streams each runner object (newline-delimited) across pages.
fn fetch_all_runners(actions: &GitHubActions, slug: &str) -> Result<Vec<ApiRunner>, CliFailure> {
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{slug}/actions/runners?per_page=100"),
            "--jq".to_owned(),
            ".runners[]".to_owned(),
        ])
        .map_err(|e| CliFailure::new(2, format!("failed to list runners for {slug}: {e}")))?;
    let mut runners = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let runner: ApiRunner = serde_json::from_str(line)
            .map_err(|e| CliFailure::new(2, format!("runner JSON parse failed for {slug}: {e}")))?;
        runners.push(runner);
    }
    Ok(runners)
}

/// Fleet-wide Actions runner pin. Services opt out of automatic updates so
/// every host keeps the same reviewed runner behavior.
const PINNED_RUNNER_VERSION: &str = "2.335.1";
const PINNED_RUNNER_SHA256: &str =
    "e1a9bc7a3661e06fa0b129d15c2064fe65dc81a431001d8958a9db1409b73769";
const PINNED_RUSTUP_VERSION: &str = "1.29.0";
const PINNED_RUSTUP_SHA256: &str =
    "aeb4105778ca1bd3c6b0e75768f581c656633cd51368fa61289b6a71696ac7e1";

fn runner_package_url() -> (String, String) {
    let name = format!("actions-runner-osx-arm64-{PINNED_RUNNER_VERSION}.tar.gz");
    let url = format!(
        "https://github.com/actions/runner/releases/download/v{PINNED_RUNNER_VERSION}/{name}"
    );
    (name, url)
}

fn verify_sha256(path: &Path, expected: &str, what: &str) -> Result<(), CliFailure> {
    let file = fs::File::open(path)
        .map_err(|e| CliFailure::new(1, format!("failed to read {what}: {e}")))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let count = reader
            .read(&mut chunk)
            .map_err(|e| CliFailure::new(1, format!("failed to hash {what}: {e}")))?;
        if count == 0 {
            break;
        }
        hasher.update(&chunk[..count]);
    }
    let actual = hex::encode(hasher.finalize());
    if actual == expected {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{what} SHA-256 mismatch: got {actual}, expected {expected}"),
        ))
    }
}

fn verify_runner_package(path: &Path) -> Result<(), CliFailure> {
    verify_sha256(path, PINNED_RUNNER_SHA256, "runner package")
}

// ---------- tag ----------

fn machine_tag_path(state_dir: &Path) -> PathBuf {
    state_dir.join("machine-tag")
}

/// Read this box's stored machine tag, if any.
fn read_stored_tag(state_dir: &Path) -> Option<String> {
    let raw = fs::read_to_string(machine_tag_path(state_dir)).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// `shipyard runner tag [--set <tag>]`.
pub(super) fn tag_command<W: Write>(
    state_dir: &Path,
    set: Option<String>,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if let Some(tag) = set {
        let tag = tag.trim().to_owned();
        validate_machine_tag(&tag).map_err(|reason| CliFailure::new(2, reason))?;
        let path = machine_tag_path(state_dir);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| CliFailure::new(1, format!("failed to create state dir: {e}")))?;
        }
        fs::write(&path, format!("{tag}\n"))
            .map_err(|e| CliFailure::new(1, format!("failed to write machine tag: {e}")))?;
        if json {
            let mut data = BTreeMap::new();
            data.insert("machine_tag".to_owned(), Value::from(tag.clone()));
            data.insert("path".to_owned(), Value::from(path.display().to_string()));
            envelope(stdout, "runner.tag", data)?;
        } else {
            writeln!(stdout, "machine tag set to `{tag}` ({})", path.display()).ok();
        }
        return Ok(ExitCode::SUCCESS);
    }

    match read_stored_tag(state_dir) {
        Some(tag) => {
            if json {
                let mut data = BTreeMap::new();
                data.insert("machine_tag".to_owned(), Value::from(tag.clone()));
                envelope(stdout, "runner.tag", data)?;
            } else {
                writeln!(stdout, "{tag}").ok();
            }
            Ok(ExitCode::SUCCESS)
        }
        None => Err(CliFailure::new(
            1,
            "No machine tag set. Set one with `shipyard runner tag --set <studio|m1|m5>`.",
        )),
    }
}

// ---------- register ----------

/// Inputs for [`register_command`].
pub(super) struct RegisterArgs<'a> {
    pub cwd: &'a Path,
    pub state_dir: &'a Path,
    pub actions: &'a GitHubActions,
    pub repo: Option<String>,
    pub count: u32,
    pub machine_tag: Option<String>,
    pub labels: Vec<String>,
    pub ci_root: Option<PathBuf>,
    pub dry_run: bool,
    pub json: bool,
}

fn default_ci_root() -> PathBuf {
    home_dir().join("actions-ci")
}

fn home_dir() -> PathBuf {
    std::env::var("HOME").map_or_else(|_| PathBuf::from("."), PathBuf::from)
}

fn cpu_count() -> usize {
    Command::new("sysctl")
        .args(["-n", "hw.ncpu"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(4)
}

fn parallel_per_runner(cpus: usize, participating_runners: usize) -> usize {
    (cpus / participating_runners.max(1)).max(1)
}

fn configured_parallel(runner_dir: &Path) -> Result<usize, CliFailure> {
    let path = runner_dir.join(".env");
    let raw = fs::read_to_string(&path).map_err(|error| {
        CliFailure::new(
            3,
            format!(
                "cannot reserve capacity for deferred runner at {}: failed to read {}: {error}",
                runner_dir.display(),
                path.display()
            ),
        )
    })?;
    let value = raw
        .lines()
        .find_map(|line| line.strip_prefix("CMAKE_BUILD_PARALLEL_LEVEL="))
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            CliFailure::new(
                3,
                format!(
                    "cannot reserve capacity for deferred runner at {}: {} has no positive CMAKE_BUILD_PARALLEL_LEVEL",
                    runner_dir.display(),
                    path.display()
                ),
            )
        })?;
    Ok(value)
}

fn external_runner_parallel(home: &Path, plan: &[RunnerPlan]) -> Result<usize, CliFailure> {
    let entries = fs::read_dir(home).map_err(|error| {
        CliFailure::new(
            1,
            format!("failed to inspect local runner directories: {error}"),
        )
    })?;
    let mut reserved = 0_usize;
    for entry in entries {
        let entry = entry.map_err(|error| {
            CliFailure::new(1, format!("failed to inspect local runner entry: {error}"))
        })?;
        let path = entry.path();
        let is_runner_dir = entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("actions-runner-"));
        if !is_runner_dir
            || !path.is_dir()
            || !path.join(".runner").is_file()
            || plan.iter().any(|planned| planned.dir == path)
        {
            continue;
        }
        reserved = reserved.saturating_add(configured_parallel(&path)?);
    }
    Ok(reserved)
}

/// The per-runner `.env` the GitHub Actions service loads: points jobs at the
/// shared caches and isolates each runner's ccache base path so cross-worktree
/// hits work (`CCACHE_BASEDIR` + `CCACHE_NOHASHDIR`). Cache *size* is owned by
/// the host's `ccache.conf`, not set here.
fn runner_env_file(ci_root: &Path, work: &Path, runner_dir: &Path, parallel: usize) -> String {
    let cache = ci_root.join("cache");
    let toolcache = runner_dir.join("_toolcache");
    format!(
        "CCACHE_DIR={ccache}\n\
         CCACHE_BASEDIR={work}\n\
         CCACHE_NOHASHDIR=true\n\
         CCACHE_NODEPEND=true\n\
         CCACHE_COMPILERCHECK=content\n\
         CCACHE_SLOPPINESS=time_macros,pch_defines\n\
         CMAKE_BUILD_PARALLEL_LEVEL={parallel}\n\
         CTEST_PARALLEL_LEVEL={parallel}\n\
         FETCHCONTENT_BASE_DIR={fetchcontent}\n\
         RUSTUP_HOME={rustup}\n\
         CARGO_HOME={cargo}\n",
        ccache = cache.join("ccache").display(),
        work = work.display(),
        fetchcontent = cache.join("fetchcontent-src").display(),
        rustup = toolcache.join("rustup").display(),
        cargo = toolcache.join("cargo").display(),
    )
}

fn runner_path_file(runner_dir: &Path) -> String {
    let cargo_bin = runner_dir.join("_toolcache/cargo/bin");
    let local_bin = home_dir().join(".local/bin");
    format!(
        "/usr/bin:/bin:/usr/sbin:/sbin:{}:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:{}\n",
        cargo_bin.display(),
        local_bin.display()
    )
}

fn private_rust_is_ready(runner_dir: &Path) -> bool {
    let rustup_home = runner_dir.join("_toolcache/rustup");
    let cargo_home = runner_dir.join("_toolcache/cargo");
    let private_path = runner_path_file(runner_dir);
    let probe = |binary: &Path, args: &[&str]| {
        Command::new(binary)
            .args(args)
            .env("RUSTUP_HOME", &rustup_home)
            .env("CARGO_HOME", &cargo_home)
            .env("PATH", private_path.trim())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    probe(&cargo_home.join("bin/cargo"), &["--version"])
        && probe(
            &cargo_home.join("bin/rustup"),
            &["show", "active-toolchain"],
        )
}

fn installed_runner_version(runner_dir: &Path) -> Option<String> {
    Command::new(runner_dir.join("bin/Runner.Listener"))
        .arg("--version")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|version| version.trim().to_owned())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RunnerInstallation {
    configured: bool,
    service_installed: bool,
}

fn inspect_runner_installation(runner_dir: &Path) -> RunnerInstallation {
    RunnerInstallation {
        configured: runner_dir.join(".runner").is_file(),
        service_installed: runner_dir.join(".service").is_file(),
    }
}

fn require_service_less_at_boundary(
    runner_dir: &Path,
    runner_name: &str,
    boundary: &str,
) -> Result<RunnerInstallation, CliFailure> {
    let installation = inspect_runner_installation(runner_dir);
    if installation.configured && installation.service_installed {
        return Err(CliFailure::new(
            3,
            format!(
                "runner `{runner_name}` gained a service {boundary}; deferring without modifying it"
            ),
        ));
    }
    Ok(installation)
}

fn validate_installation_shape(
    runner_dir: &Path,
    installation: RunnerInstallation,
) -> Result<(), CliFailure> {
    if !installation.configured && installation.service_installed {
        return Err(CliFailure::new(
            1,
            format!(
                "runner service exists without configuration at {}; repair or remove the partial installation before retrying",
                runner_dir.display()
            ),
        ));
    }
    if runner_dir.exists() && !installation.configured {
        return Err(CliFailure::new(
            1,
            format!(
                "unconfigured runner directory {} already exists; inspect or remove the partial installation before retrying",
                runner_dir.display()
            ),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RunnerIdentity {
    agent_name: String,
    repo_slug: String,
}

fn read_runner_identity(runner_dir: &Path) -> Result<RunnerIdentity, CliFailure> {
    let path = runner_dir.join(".runner");
    let raw = fs::read_to_string(&path).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "failed to read runner identity at {}: {error}",
                path.display()
            ),
        )
    })?;
    let (agent_name, repo_slug) = parse_dot_runner(&raw).ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "runner identity at {} is invalid; repair the .runner file before retrying",
                path.display()
            ),
        )
    })?;
    Ok(RunnerIdentity {
        agent_name,
        repo_slug,
    })
}

fn validate_planned_runner_identity(
    runner_dir: &Path,
    runner_name: &str,
    expected_repo_slug: &str,
    installation: RunnerInstallation,
    registered_runner: Option<&ApiRunner>,
) -> Result<(), CliFailure> {
    if !installation.configured {
        return Ok(());
    }
    let identity = read_runner_identity(runner_dir)?;
    if identity.agent_name != runner_name {
        return Err(CliFailure::new(
            1,
            format!(
                "runner identity mismatch at {}: directory plan expects `{runner_name}`, .runner names `{}`",
                runner_dir.display(),
                identity.agent_name
            ),
        ));
    }
    if !identity.repo_slug.eq_ignore_ascii_case(expected_repo_slug) {
        return Err(CliFailure::new(
            1,
            format!(
                "runner repository mismatch at {}: requested `{expected_repo_slug}`, .runner targets `{}`",
                runner_dir.display(),
                identity.repo_slug
            ),
        ));
    }
    if registered_runner.is_none() {
        return Err(CliFailure::new(
            1,
            format!(
                "runner configuration at {} is not registered in GitHub as {}; remove or repair the stale local installation before retrying",
                runner_dir.display(),
                runner_name
            ),
        ));
    }
    Ok(())
}

fn runner_by_name<'a>(runners: &'a [ApiRunner], name: &str) -> Option<&'a ApiRunner> {
    runners.iter().find(|runner| runner.name == name)
}

fn require_offline_idle_runner<'a>(
    runners: &'a [ApiRunner],
    runner_name: &str,
) -> Result<&'a ApiRunner, CliFailure> {
    let runner = runner_by_name(runners, runner_name).ok_or_else(|| {
        CliFailure::new(
            1,
            format!(
                "runner `{runner_name}` disappeared from GitHub inventory; refusing to modify its installation"
            ),
        )
    })?;
    if !runner.status.eq_ignore_ascii_case("online")
        && !runner.status.eq_ignore_ascii_case("offline")
    {
        return Err(CliFailure::new(
            1,
            format!(
                "runner `{runner_name}` has unknown GitHub status `{}`; refusing to modify its installation",
                runner.status
            ),
        ));
    }
    if runner.busy {
        return Err(CliFailure::new(
            3,
            format!(
                "runner `{runner_name}` is {}/busy; deferring its upgrade without stopping the service",
                runner.status
            ),
        ));
    }
    if runner.status.eq_ignore_ascii_case("online") {
        return Err(CliFailure::new(
            3,
            format!(
                "runner `{runner_name}` is online without a Shipyard-managed service; deferring its upgrade because another process may be using the installation"
            ),
        ));
    }
    Ok(runner)
}

fn sibling_transaction_path(runner_dir: &Path, suffix: &str) -> Result<PathBuf, CliFailure> {
    let parent = runner_dir.parent().ok_or_else(|| {
        CliFailure::new(
            1,
            format!("runner directory {} has no parent", runner_dir.display()),
        )
    })?;
    let name = runner_dir
        .file_name()
        .and_then(OsStr::to_str)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!(
                    "runner directory {} has no UTF-8 name",
                    runner_dir.display()
                ),
            )
        })?;
    Ok(parent.join(format!(".{name}.shipyard-{suffix}")))
}

fn clone_runner_installation(source: &Path, destination: &Path) -> Result<(), CliFailure> {
    if destination.exists() {
        return Err(CliFailure::new(
            1,
            format!(
                "stale runner transaction path {} exists; inspect and remove it before retrying",
                destination.display()
            ),
        ));
    }
    let mut command = Command::new("/bin/cp");
    #[cfg(target_os = "macos")]
    command.arg(OsStr::new("-cR"));
    #[cfg(not(target_os = "macos"))]
    command.arg(OsStr::new("-R"));
    let status = command
        .args([source.as_os_str(), destination.as_os_str()])
        .status()
        .map_err(|error| CliFailure::new(1, format!("failed to stage runner clone: {error}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("stage runner clone failed (exit {:?})", status.code()),
        ))
    }
}

fn prepare_staged_runner(
    runner_dir: &Path,
    package: &Path,
    installation: RunnerInstallation,
    ci_root: &Path,
    work: &Path,
    parallel: usize,
) -> Result<PathBuf, CliFailure> {
    let staged = sibling_transaction_path(runner_dir, "stage")?;
    if installation.configured {
        clone_runner_installation(runner_dir, &staged)?;
    } else {
        fs::create_dir(&staged).map_err(|error| {
            CliFailure::new(
                1,
                format!("failed to create runner staging directory: {error}"),
            )
        })?;
    }
    let result = (|| -> Result<(), CliFailure> {
        run(
            "/usr/bin/tar",
            &[
                "xzf",
                &package.to_string_lossy(),
                "-C",
                &staged.to_string_lossy(),
            ],
            "extract staged runner",
        )?;
        if installed_runner_version(&staged).as_deref() != Some(PINNED_RUNNER_VERSION) {
            return Err(CliFailure::new(
                1,
                format!("staged runner is not pinned version {PINNED_RUNNER_VERSION}"),
            ));
        }
        fs::write(
            staged.join(".env"),
            runner_env_file(ci_root, work, runner_dir, parallel),
        )
        .map_err(|error| CliFailure::new(1, format!("failed to write staged .env: {error}")))?;
        ensure_private_rust_toolchain(&staged)?;
        fs::write(staged.join(".path"), runner_path_file(runner_dir)).map_err(|error| {
            CliFailure::new(1, format!("failed to write staged .path: {error}"))
        })?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staged);
        return Err(error);
    }
    Ok(staged)
}

fn restore_original_runner(runner_dir: &Path, backup: &Path, original: CliFailure) -> CliFailure {
    let Some(failed) = (0_u8..100).find_map(|attempt| {
        sibling_transaction_path(
            runner_dir,
            &format!("failed-{}-{attempt}", std::process::id()),
        )
        .ok()
        .filter(|path| !path.exists())
    }) else {
        return CliFailure::new(
            original.code,
            format!(
                "{}; could not reserve a unique failed-upgrade recovery path",
                original.message()
            ),
        );
    };
    let recovery = (|| -> Result<(), CliFailure> {
        if runner_dir.exists() {
            fs::rename(runner_dir, &failed).map_err(|error| {
                CliFailure::new(1, format!("failed to quarantine replacement: {error}"))
            })?;
        }
        fs::rename(backup, runner_dir).map_err(|error| {
            CliFailure::new(1, format!("failed to restore original runner: {error}"))
        })?;
        let _ = fs::remove_dir_all(&failed);
        Ok(())
    })();
    match recovery {
        Ok(()) => original,
        Err(recovery_error) => CliFailure::new(
            original.code,
            format!(
                "{}; original runner recovery also failed: {}",
                original.message(),
                recovery_error.message()
            ),
        ),
    }
}

fn activate_staged_service_install(runner_dir: &Path, staged: &Path) -> Result<(), CliFailure> {
    let backup = sibling_transaction_path(runner_dir, "backup")?;
    if backup.exists() {
        return Err(CliFailure::new(
            1,
            format!(
                "stale runner backup {} exists; inspect it before retrying",
                backup.display()
            ),
        ));
    }
    fs::rename(runner_dir, &backup).map_err(|error| {
        CliFailure::new(1, format!("failed to preserve configured runner: {error}"))
    })?;
    if let Err(error) = fs::rename(staged, runner_dir) {
        let original = CliFailure::new(1, format!("failed to activate staged runner: {error}"));
        return Err(restore_original_runner(runner_dir, &backup, original));
    }
    let install_result = run_in(
        runner_dir,
        "./svc.sh",
        &["install"],
        "install runner service",
    );
    if let Err(error) = install_result {
        // `svc.sh install` may have created external launchd state even when
        // it returned failure. Do not restore the old directory underneath
        // an ambiguous service registration unless cleanup succeeds.
        if let Err(uninstall) = run_in(
            runner_dir,
            "./svc.sh",
            &["uninstall"],
            "uninstall failed replacement service",
        ) {
            return Err(CliFailure::new(
                error.code,
                format!(
                    "{}; replacement service cleanup also failed: {}; original remains preserved at {}",
                    error.message(),
                    uninstall.message(),
                    backup.display()
                ),
            ));
        }
        return Err(restore_original_runner(runner_dir, &backup, error));
    }
    if let Err(error) = run_in(runner_dir, "./svc.sh", &["start"], "start runner service") {
        // A nonzero start may still have launched Listener/Worker. The runner's
        // `svc.sh uninstall` is the single compound stop+uninstall operation;
        // invoking `svc.sh stop` first makes its internal second stop fail on
        // an already-unloaded LaunchAgent.
        if let Err(uninstall) = run_in(
            runner_dir,
            "./svc.sh",
            &["uninstall"],
            "uninstall failed replacement service",
        ) {
            return Err(CliFailure::new(
                error.code,
                format!(
                    "{}; replacement service cleanup also failed: {}; original remains preserved at {}",
                    error.message(),
                    uninstall.message(),
                    backup.display()
                ),
            ));
        }
        return Err(restore_original_runner(runner_dir, &backup, error));
    }
    fs::remove_dir_all(&backup)
        .map_err(|error| CliFailure::new(1, format!("failed to remove runner backup: {error}")))?;
    Ok(())
}

fn ensure_private_rust_toolchain(runner_dir: &Path) -> Result<(), CliFailure> {
    let toolcache = runner_dir.join("_toolcache");
    let rustup_home = toolcache.join("rustup");
    let cargo_home = toolcache.join("cargo");
    fs::create_dir_all(&rustup_home)
        .map_err(|e| CliFailure::new(1, format!("failed to create private rustup home: {e}")))?;
    fs::create_dir_all(&cargo_home)
        .map_err(|e| CliFailure::new(1, format!("failed to create private cargo home: {e}")))?;

    if private_rust_is_ready(runner_dir) {
        return Ok(());
    }

    let installer = toolcache.join("rustup-init");
    let rustup_url = format!(
        "https://static.rust-lang.org/rustup/archive/{PINNED_RUSTUP_VERSION}/aarch64-apple-darwin/rustup-init"
    );
    run(
        "/usr/bin/curl",
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "-fsSL",
            &rustup_url,
            "-o",
            &installer.to_string_lossy(),
        ],
        "download rustup installer",
    )?;
    verify_sha256(&installer, PINNED_RUSTUP_SHA256, "rustup-init")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = fs::metadata(&installer)
            .map_err(|e| CliFailure::new(1, format!("failed to inspect rustup-init: {e}")))?
            .permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&installer, permissions).map_err(|e| {
            CliFailure::new(1, format!("failed to make rustup-init executable: {e}"))
        })?;
    }
    let status = Command::new(&installer)
        .args([
            "-y",
            "--no-modify-path",
            "--profile",
            "minimal",
            "--default-toolchain",
            "stable-aarch64-apple-darwin",
        ])
        .env("RUSTUP_HOME", &rustup_home)
        .env("CARGO_HOME", &cargo_home)
        .env("PATH", runner_path_file(runner_dir).trim())
        .status()
        .map_err(|e| CliFailure::new(1, format!("failed to run rustup installer: {e}")))?;
    let _ = fs::remove_file(installer);
    if !status.success() {
        return Err(CliFailure::new(
            1,
            format!("private rustup install failed (exit {:?})", status.code()),
        ));
    }
    if !private_rust_is_ready(runner_dir) {
        return Err(CliFailure::new(
            1,
            "private cargo/rustup toolchain is not runnable after install",
        ));
    }
    Ok(())
}

/// `shipyard runner register`.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)]
pub(super) fn register_command<W: Write>(
    args: RegisterArgs,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if args.count == 0 {
        return Err(CliFailure::new(2, "--count must be at least 1"));
    }
    let slug = resolve_repo_slug(args.repo.clone(), args.cwd)?;
    let repo_short = short_repo(&slug).to_owned();

    let tag = match args.machine_tag.clone() {
        Some(tag) => {
            let tag = tag.trim().to_owned();
            validate_machine_tag(&tag).map_err(|reason| CliFailure::new(2, reason))?;
            tag
        }
        None => read_stored_tag(args.state_dir).ok_or_else(|| {
            CliFailure::new(
                1,
                "No machine tag set. Pass --machine-tag or run `shipyard runner tag --set <tag>`.",
            )
        })?,
    };
    validate_machine_tag(&tag).map_err(|reason| CliFailure::new(2, reason))?;

    let labels = if args.labels.is_empty() {
        default_labels(&repo_short, &tag)
    } else {
        args.labels.clone()
    };
    let labels_csv = labels.join(",");
    let ci_root = args.ci_root.clone().unwrap_or_else(default_ci_root);

    let existing = fetch_all_runners(args.actions, &slug)?;

    // Preserve `--count` as the documented additive registration count while
    // also including this host's existing configured runners for pin upgrades.
    let home = home_dir();
    let mut plan = build_runner_plan(&existing, &repo_short, &tag, args.count, &ci_root, &home);
    let mut deferred = BTreeMap::new();
    let mut retained = BTreeMap::new();
    for entry in &mut plan {
        let installation = inspect_runner_installation(&entry.dir);
        validate_planned_runner_identity(
            &entry.dir,
            &entry.name,
            &slug,
            installation,
            entry.registered.as_ref(),
        )?;
        validate_installation_shape(&entry.dir, installation)?;
        if installation.configured {
            entry.registered.as_ref().ok_or_else(|| {
                CliFailure::new(
                    1,
                    format!(
                        "configured runner `{}` has no retained GitHub status; refusing to modify it",
                        entry.name
                    ),
                )
            })?;
            if installation.service_installed {
                if installed_runner_version(&entry.dir).as_deref() == Some(PINNED_RUNNER_VERSION) {
                    retained.insert(entry.name.clone(), "pinned_service".to_owned());
                } else {
                    deferred.insert(entry.name.clone(), "service_installed".to_owned());
                }
                continue;
            }
            let refreshed = fetch_all_runners(args.actions, &slug)?;
            if let Some(observed) = runner_by_name(&refreshed, &entry.name) {
                entry.registered = Some(observed.clone());
            }
            if let Err(error) = require_offline_idle_runner(&refreshed, &entry.name) {
                if error.code == 3 {
                    let reason = if runner_by_name(&refreshed, &entry.name)
                        .is_some_and(|runner| runner.busy)
                    {
                        "busy"
                    } else {
                        "online_without_service"
                    };
                    deferred.insert(entry.name.clone(), reason.to_owned());
                    continue;
                }
                return Err(error);
            }
        }
    }

    let mut unchanged = deferred.clone();
    unchanged.extend(retained.clone());
    // Existing runners may become active after any API snapshot. Preserve and
    // reserve every configured runner's current allocation; only genuinely
    // additive runners divide the remaining capacity. This keeps a late
    // deferral from invalidating allocations already activated earlier.
    let mut reserved_allocations = unchanged.clone();
    for entry in &plan {
        if entry.registered.is_some() {
            reserved_allocations
                .entry(entry.name.clone())
                .or_insert_with(|| "existing_allocation".to_owned());
        }
    }
    let external_reserved = external_runner_parallel(&home, &plan)?;
    let available_to_plan = cpu_count().checked_sub(external_reserved).ok_or_else(|| {
        CliFailure::new(
            3,
            format!(
                "other local runners reserve {external_reserved} build slots, exceeding this host's detected CPU capacity; reconcile local allocations before adding runners"
            ),
        )
    })?;
    let parallel = allocate_plan_parallel(&mut plan, &reserved_allocations, available_to_plan)?;

    if args.dry_run {
        report_register(
            stdout,
            args.json,
            &slug,
            &tag,
            &labels_csv,
            &ci_root,
            parallel,
            &plan,
            &deferred,
            &retained,
            true,
        )?;
        return Ok(if deferred.is_empty() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(3)
        });
    }

    // Cache the fleet-wide pinned runner tarball once. `config.sh
    // --disableupdate` below keeps GitHub from silently replacing it later.
    let (pkg_name, pkg_url) = runner_package_url();
    let pkg_cache = ci_root.join("cache").join("actions-runner-pkg");
    fs::create_dir_all(&pkg_cache)
        .map_err(|e| CliFailure::new(1, format!("failed to create package cache: {e}")))?;
    let pkg_path = pkg_cache.join(pkg_name);
    if !pkg_path.exists() {
        let partial = pkg_path.with_extension("partial");
        run(
            "/usr/bin/curl",
            &[
                "--proto",
                "=https",
                "--tlsv1.2",
                "-fsSL",
                "-o",
                &partial.to_string_lossy(),
                &pkg_url,
            ],
            "download runner",
        )?;
        verify_runner_package(&partial)?;
        fs::rename(&partial, &pkg_path)
            .map_err(|e| CliFailure::new(1, format!("failed to cache runner package: {e}")))?;
    }
    verify_runner_package(&pkg_path)?;

    // Close the long package-download window before any runner is changed.
    // A newly installed service transfers control away from this command.
    for entry in &plan {
        if !unchanged.contains_key(&entry.name)
            && require_service_less_at_boundary(&entry.dir, &entry.name, "after preflight").is_err()
        {
            deferred.insert(entry.name.clone(), "service_installed".to_owned());
            unchanged.insert(entry.name.clone(), "service_installed".to_owned());
        }
    }

    for entry in &mut plan {
        let installation = inspect_runner_installation(&entry.dir);
        if unchanged.contains_key(&entry.name) {
            continue;
        }
        if require_service_less_at_boundary(&entry.dir, &entry.name, "at the mutation boundary")
            .is_err()
        {
            deferred.insert(entry.name.clone(), "service_installed".to_owned());
            unchanged.insert(entry.name.clone(), "service_installed".to_owned());
            continue;
        }
        fs::create_dir_all(&entry.work)
            .map_err(|e| CliFailure::new(1, format!("failed to create work dir: {e}")))?;
        let installed_version = installed_runner_version(&entry.dir);
        let needs_upgrade = installed_version.as_deref() != Some(PINNED_RUNNER_VERSION);
        if installation.configured {
            let refreshed = fetch_all_runners(args.actions, &slug)?;
            if let Some(observed) = runner_by_name(&refreshed, &entry.name) {
                entry.registered = Some(observed.clone());
            }
            if let Err(error) = require_offline_idle_runner(&refreshed, &entry.name) {
                if error.code == 3 {
                    let reason = if runner_by_name(&refreshed, &entry.name)
                        .is_some_and(|runner| runner.busy)
                    {
                        "busy"
                    } else {
                        "online_without_service"
                    };
                    deferred.insert(entry.name.clone(), reason.to_owned());
                    unchanged.insert(entry.name.clone(), reason.to_owned());
                    continue;
                }
                return Err(error);
            }
        }
        let provision_result = (|| -> Result<(), CliFailure> {
            if needs_upgrade || installation.configured {
                let staged = prepare_staged_runner(
                    &entry.dir,
                    &pkg_path,
                    installation,
                    &ci_root,
                    &entry.work,
                    entry.parallel,
                )?;
                if let Err(error) = validate_planned_runner_identity(
                    &staged,
                    &entry.name,
                    &slug,
                    installation,
                    entry.registered.as_ref(),
                ) {
                    let _ = fs::remove_dir_all(&staged);
                    return Err(error);
                }

                if installation.configured {
                    // Staging can include archive extraction and toolchain
                    // preparation. Refresh at the final rename boundary so an
                    // offline observation made before that work cannot
                    // authorize mutation after the runner becomes active.
                    if require_service_less_at_boundary(&entry.dir, &entry.name, "during staging")
                        .is_err()
                    {
                        let _ = fs::remove_dir_all(&staged);
                        deferred.insert(entry.name.clone(), "service_installed".to_owned());
                        unchanged.insert(entry.name.clone(), "service_installed".to_owned());
                        return Ok(());
                    }
                    let refreshed = fetch_all_runners(args.actions, &slug)?;
                    if let Some(observed) = runner_by_name(&refreshed, &entry.name) {
                        entry.registered = Some(observed.clone());
                    }
                    if let Err(error) = require_offline_idle_runner(&refreshed, &entry.name) {
                        let _ = fs::remove_dir_all(&staged);
                        if error.code == 3 {
                            let reason = if runner_by_name(&refreshed, &entry.name)
                                .is_some_and(|runner| runner.busy)
                            {
                                "busy"
                            } else {
                                "online_without_service"
                            };
                            deferred.insert(entry.name.clone(), reason.to_owned());
                            unchanged.insert(entry.name.clone(), reason.to_owned());
                            return Ok(());
                        }
                        return Err(error);
                    }
                    if require_service_less_at_boundary(
                        &entry.dir,
                        &entry.name,
                        "at final activation",
                    )
                    .is_err()
                    {
                        let _ = fs::remove_dir_all(&staged);
                        deferred.insert(entry.name.clone(), "service_installed".to_owned());
                        unchanged.insert(entry.name.clone(), "service_installed".to_owned());
                        return Ok(());
                    }
                    if let Err(error) = activate_staged_service_install(&entry.dir, &staged) {
                        let _ = fs::remove_dir_all(&staged);
                        return Err(error);
                    }
                } else if let Err(error) = fs::rename(&staged, &entry.dir) {
                    let _ = fs::remove_dir_all(&staged);
                    return Err(CliFailure::new(
                        1,
                        format!("failed to activate new runner: {error}"),
                    ));
                }
            } else {
                fs::write(
                    entry.dir.join(".env"),
                    runner_env_file(&ci_root, &entry.work, &entry.dir, entry.parallel),
                )
                .map_err(|e| CliFailure::new(1, format!("failed to write .env: {e}")))?;
                ensure_private_rust_toolchain(&entry.dir)?;
                fs::write(entry.dir.join(".path"), runner_path_file(&entry.dir))
                    .map_err(|e| CliFailure::new(1, format!("failed to write .path: {e}")))?;
            }

            // Configured service-less installations were clone-staged and
            // activated above. They retain `.runner` credentials and must not
            // be passed through fresh `config.sh` registration.
            if installation.configured {
                return Ok(());
            }

            let token = args
                .actions
                .run_gh(&[
                    "api".to_owned(),
                    "-X".to_owned(),
                    "POST".to_owned(),
                    format!("repos/{slug}/actions/runners/registration-token"),
                    "--jq".to_owned(),
                    ".token".to_owned(),
                ])
                .map_err(|e| CliFailure::new(2, format!("failed to get registration token: {e}")))?
                .trim()
                .to_owned();

            let config_args = runner_config_args(&slug, &token, entry, &labels_csv);
            let config_arg_refs: Vec<&str> = config_args.iter().map(String::as_str).collect();
            run_in(
                &entry.dir,
                "./config.sh",
                &config_arg_refs,
                "configure runner",
            )?;
            run_in(
                &entry.dir,
                "./svc.sh",
                &["install"],
                "install runner service",
            )?;
            run_in(&entry.dir, "./svc.sh", &["start"], "start runner service")?;
            Ok(())
        })();
        provision_result?;
    }

    report_register(
        stdout,
        args.json,
        &slug,
        &tag,
        &labels_csv,
        &ci_root,
        parallel,
        &plan,
        &deferred,
        &retained,
        false,
    )?;
    if deferred.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        if !args.json {
            let summary = deferred
                .iter()
                .map(|(name, reason)| format!("{name} ({reason})"))
                .collect::<Vec<_>>()
                .join(", ");
            writeln!(stdout, "Deferred existing runner upgrade(s): {summary}").ok();
        }
        Ok(ExitCode::from(3))
    }
}

#[derive(Clone)]
struct RunnerPlan {
    name: String,
    dir: PathBuf,
    work: PathBuf,
    registered: Option<ApiRunner>,
    parallel: usize,
}

fn allocate_plan_parallel(
    plan: &mut [RunnerPlan],
    deferred: &BTreeMap<String, String>,
    cpus: usize,
) -> Result<usize, CliFailure> {
    let mut reserved_parallel = 0_usize;
    for entry in &mut *plan {
        if deferred.contains_key(&entry.name) {
            entry.parallel = configured_parallel(&entry.dir)?;
            reserved_parallel = reserved_parallel.saturating_add(entry.parallel);
        }
    }
    let mutable_count = plan.len().saturating_sub(deferred.len());
    let available_parallel = cpus.saturating_sub(reserved_parallel);
    if mutable_count > 0 && available_parallel < mutable_count {
        return Err(CliFailure::new(
            3,
            format!(
                "deferred runners reserve {reserved_parallel} build slots, leaving {available_parallel} for {mutable_count} eligible runner(s); drain and reconcile existing runners before adding capacity"
            ),
        ));
    }
    let parallel = parallel_per_runner(available_parallel, mutable_count);
    for entry in plan {
        if !deferred.contains_key(&entry.name) {
            entry.parallel = parallel;
        }
    }
    Ok(parallel)
}

fn build_runner_plan(
    registered_runners: &[ApiRunner],
    repo_short: &str,
    machine_tag: &str,
    add_count: u32,
    ci_root: &Path,
    home: &Path,
) -> Vec<RunnerPlan> {
    let prefix = format!("{repo_short}-{machine_tag}-");
    let registered_names: Vec<String> = registered_runners
        .iter()
        .map(|runner| runner.name.clone())
        .collect();
    let mut existing: Vec<ApiRunner> = registered_runners
        .iter()
        .filter(|runner| runner.name.starts_with(&prefix))
        .filter(|runner| {
            home.join(format!("actions-runner-{}", runner.name))
                .join(".runner")
                .is_file()
        })
        .cloned()
        .collect();
    existing.sort_by(|left, right| left.name.cmp(&right.name));

    let mut plan: Vec<RunnerPlan> = existing
        .into_iter()
        .map(|runner| {
            let name = runner.name.clone();
            RunnerPlan {
                work: ci_root.join("work").join(&name),
                dir: home.join(format!("actions-runner-{name}")),
                name,
                registered: Some(runner),
                parallel: 0,
            }
        })
        .collect();
    let start = next_index(&registered_names, repo_short, machine_tag);
    for next in (start..).take(add_count as usize) {
        let name = runner_name(repo_short, machine_tag, next);
        plan.push(RunnerPlan {
            work: ci_root.join("work").join(&name),
            dir: home.join(format!("actions-runner-{name}")),
            name,
            registered: None,
            parallel: 0,
        });
    }
    plan
}

fn runner_config_args(
    slug: &str,
    token: &str,
    entry: &RunnerPlan,
    labels_csv: &str,
) -> Vec<String> {
    vec![
        "--unattended".to_owned(),
        "--replace".to_owned(),
        "--url".to_owned(),
        format!("https://github.com/{slug}"),
        "--token".to_owned(),
        token.to_owned(),
        "--name".to_owned(),
        entry.name.clone(),
        "--labels".to_owned(),
        labels_csv.to_owned(),
        "--work".to_owned(),
        entry.work.display().to_string(),
        "--disableupdate".to_owned(),
    ]
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn report_register<W: Write>(
    stdout: &mut W,
    json: bool,
    slug: &str,
    tag: &str,
    labels_csv: &str,
    ci_root: &Path,
    parallel: usize,
    plan: &[RunnerPlan],
    deferred: &BTreeMap<String, String>,
    retained: &BTreeMap<String, String>,
    dry_run: bool,
) -> Result<ExitCode, CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("repo".to_owned(), Value::from(slug.to_owned()));
        data.insert("machine_tag".to_owned(), Value::from(tag.to_owned()));
        data.insert("labels".to_owned(), Value::from(labels_csv.to_owned()));
        data.insert("dry_run".to_owned(), Value::from(dry_run));
        data.insert(
            "parallel_per_runner".to_owned(),
            Value::from(parallel as u64),
        );
        data.insert(
            "deferred_runners".to_owned(),
            serde_json::to_value(deferred).unwrap_or(Value::Null),
        );
        data.insert(
            "retained_runners".to_owned(),
            serde_json::to_value(retained).unwrap_or(Value::Null),
        );
        let runners: Vec<Value> = plan
            .iter()
            .map(|p| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Value::from(p.name.clone()));
                m.insert("dir".to_owned(), Value::from(p.dir.display().to_string()));
                m.insert("work".to_owned(), Value::from(p.work.display().to_string()));
                m.insert("parallel".to_owned(), Value::from(p.parallel as u64));
                if let Some(runner) = &p.registered {
                    m.insert("status".to_owned(), Value::from(runner.status.clone()));
                    m.insert("busy".to_owned(), Value::from(runner.busy));
                    let deferred_reason = deferred.get(&p.name).map(String::as_str);
                    m.insert(
                        "action".to_owned(),
                        Value::from(if let Some(reason) = deferred_reason {
                            format!("defer_{reason}")
                        } else if let Some(reason) = retained.get(&p.name) {
                            format!("retain_{reason}")
                        } else {
                            "upgrade_or_refresh".to_owned()
                        }),
                    );
                } else {
                    m.insert("status".to_owned(), Value::Null);
                    m.insert("busy".to_owned(), Value::from(false));
                    m.insert("action".to_owned(), Value::from("register"));
                }
                Value::Object(m)
            })
            .collect();
        data.insert("runners".to_owned(), Value::from(runners));
        envelope(stdout, "runner.register", data)?;
        return Ok(ExitCode::SUCCESS);
    }

    let verb = if dry_run {
        "Would register"
    } else {
        "Registered"
    };
    writeln!(
        stdout,
        "{verb} {} runner(s) for {slug} [tag={tag}, ~{parallel} cores per eligible runner]",
        plan.len()
    )
    .ok();
    writeln!(stdout, "  labels:  {labels_csv}").ok();
    writeln!(stdout, "  ci-root: {}", ci_root.display()).ok();
    for p in plan {
        let state = p.registered.as_ref().map_or_else(
            || "new/register".to_owned(),
            |runner| {
                let deferred = deferred.contains_key(&p.name);
                let retained = retained.contains_key(&p.name);
                format!(
                    "{}/{}:{}",
                    runner.status,
                    if runner.busy { "busy" } else { "idle" },
                    if deferred {
                        "defer"
                    } else if retained {
                        "retain"
                    } else {
                        "upgrade"
                    }
                )
            },
        );
        writeln!(
            stdout,
            "  - {}  (work={}, parallel={}, {state})",
            p.name,
            p.work.display(),
            p.parallel
        )
        .ok();
    }
    if dry_run {
        writeln!(stdout, "\nRe-run without --dry-run to apply.").ok();
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- list ----------

struct LocalRunner {
    name: String,
    repo_slug: String,
}

/// Parse a runner `.runner` config file, returning `(agent_name, repo_slug)`.
fn parse_dot_runner(raw: &str) -> Option<(String, String)> {
    // `.runner` files are written with a UTF-8 BOM; strip it before parsing.
    let cleaned = raw.trim_start_matches('\u{feff}').trim_start();
    let value: Value = serde_json::from_str(cleaned).ok()?;
    let name = value.get("agentName")?.as_str()?.to_owned();
    let url = value.get("gitHubUrl")?.as_str()?;
    let slug = parse_github_repo_slug(url)?;
    Some((name, slug))
}

/// Discover configured runners from this machine's `~/actions-runner*` dirs.
fn scan_local_runners() -> Vec<LocalRunner> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(home_dir()) else {
        return found;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("actions-runner") {
            continue;
        }
        let dot = entry.path().join(".runner");
        let Ok(raw) = fs::read_to_string(&dot) else {
            continue;
        };
        if let Some((agent, slug)) = parse_dot_runner(&raw) {
            found.push(LocalRunner {
                name: agent,
                repo_slug: slug,
            });
        }
    }
    found
}

/// `shipyard runner list`.
pub(super) fn list_command<W: Write>(
    cwd: &Path,
    actions: &GitHubActions,
    repo: &[String],
    all_repos: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let locals = scan_local_runners();

    let mut slugs: Vec<String> = Vec::new();
    let push = |slug: String, slugs: &mut Vec<String>| {
        if !slug.is_empty() && !slugs.iter().any(|s| s.eq_ignore_ascii_case(&slug)) {
            slugs.push(slug);
        }
    };
    for r in repo {
        push(r.clone(), &mut slugs);
    }
    if repo.is_empty() || all_repos {
        for local in &locals {
            push(local.repo_slug.clone(), &mut slugs);
        }
    }
    if let Ok(current) = resolve_repo_slug(None, cwd) {
        push(current, &mut slugs);
    }
    if slugs.is_empty() {
        return Err(CliFailure::new(
            1,
            "No repos to query. Pass --repo OWNER/REPO, or run where local runner dirs exist.",
        ));
    }

    let mut rows: Vec<PoolRow> = Vec::new();
    let mut github_names: Vec<String> = Vec::new();
    for slug in &slugs {
        let runners = fetch_all_runners(actions, slug)?;
        for r in &runners {
            github_names.push(r.name.clone());
        }
        rows.extend(pool_rows(short_repo(slug), &runners));
    }

    let local_names: Vec<String> = locals.iter().map(|l| l.name.clone()).collect();
    let orphans = orphan_local_runners(&local_names, &github_names);

    if json {
        let mut data = BTreeMap::new();
        data.insert("repos".to_owned(), Value::from(slugs.clone()));
        let row_values: Vec<Value> = rows
            .iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Value::from(r.name.clone()));
                m.insert("repo".to_owned(), Value::from(r.repo.clone()));
                m.insert("machine".to_owned(), Value::from(r.machine.clone()));
                m.insert("status".to_owned(), Value::from(r.status.clone()));
                m.insert("busy".to_owned(), Value::from(r.busy));
                m.insert("labels".to_owned(), Value::from(r.labels.clone()));
                Value::Object(m)
            })
            .collect();
        data.insert("runners".to_owned(), Value::from(row_values));
        data.insert("orphans".to_owned(), Value::from(orphans.clone()));
        envelope(stdout, "runner.list", data)?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(stdout, "{}", format_pool_table(&rows)).ok();
    if !orphans.is_empty() {
        writeln!(
            stdout,
            "\n⚠︎ {} local runner dir(s) not registered on GitHub (orphaned — remove with `shipyard runner remove`):",
            orphans.len()
        )
        .ok();
        for name in &orphans {
            writeln!(
                stdout,
                "  - {name}  (~/actions-runner-{name} or ~/actions-runner)"
            )
            .ok();
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- audit ----------

/// Resolve the repo slugs to audit, mirroring `list_command`'s resolution:
/// explicit `--repo`, local runner dirs, then the current checkout.
fn resolve_audit_slugs(cwd: &Path, repo: &[String]) -> Result<Vec<String>, CliFailure> {
    let locals = scan_local_runners();
    let mut slugs: Vec<String> = Vec::new();
    let mut push = |slug: String| {
        if !slug.is_empty() && !slugs.iter().any(|s| s.eq_ignore_ascii_case(&slug)) {
            slugs.push(slug);
        }
    };
    for r in repo {
        push(r.clone());
    }
    if repo.is_empty() {
        for local in &locals {
            push(local.repo_slug.clone());
        }
        if let Ok(current) = resolve_repo_slug(None, cwd) {
            push(current);
        }
    }
    if slugs.is_empty() {
        return Err(CliFailure::new(
            1,
            "No repos to audit. Pass --repo OWNER/REPO, or run where local runner dirs exist.",
        ));
    }
    Ok(slugs)
}

/// `shipyard runner audit` — flag host-class naming/label drift across a repo's
/// runners. Exit 0 when every runner conforms; exit 1 when any drift is found
/// (CI-friendly). Pure logic lives in [`crate::runner_provision::audit_runners`].
pub(super) fn audit_command<W: Write>(
    cwd: &Path,
    actions: &GitHubActions,
    repo: &[String],
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let slugs = resolve_audit_slugs(cwd, repo)?;

    let mut findings: Vec<(String, AuditFinding)> = Vec::new();
    for slug in &slugs {
        let runners = fetch_all_runners(actions, slug)?;
        let repo_short = short_repo(slug);
        for finding in audit_runners(repo_short, &runners) {
            findings.push((repo_short.to_owned(), finding));
        }
    }

    let with_issues = findings.iter().filter(|(_, f)| f.has_issues()).count();
    let drift = findings.iter().any(|(_, f)| f.is_drift());
    let exit = if with_issues == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    };

    if json {
        let mut data = BTreeMap::new();
        data.insert("repos".to_owned(), Value::from(slugs.clone()));
        let finding_values: Vec<Value> = findings
            .iter()
            .map(|(repo_short, f)| {
                let mut m = serde_json::Map::new();
                m.insert("name".to_owned(), Value::from(f.name.clone()));
                m.insert("repo".to_owned(), Value::from(repo_short.clone()));
                m.insert(
                    "name_class".to_owned(),
                    f.name_class.clone().map_or(Value::Null, Value::from),
                );
                m.insert(
                    "label_class".to_owned(),
                    f.label_class.clone().map_or(Value::Null, Value::from),
                );
                m.insert("ok".to_owned(), Value::from(!f.has_issues()));
                m.insert("drift".to_owned(), Value::from(f.is_drift()));
                m.insert(
                    "issues".to_owned(),
                    Value::from(
                        f.issues
                            .iter()
                            .map(|i| Value::from(i.code()))
                            .collect::<Vec<_>>(),
                    ),
                );
                Value::Object(m)
            })
            .collect();
        data.insert("findings".to_owned(), Value::from(finding_values));
        data.insert("with_issues".to_owned(), Value::from(with_issues));
        data.insert("drift".to_owned(), Value::from(drift));
        envelope(stdout, "runner.audit", data)?;
        return Ok(exit);
    }

    let bare: Vec<AuditFinding> = findings.into_iter().map(|(_, f)| f).collect();
    writeln!(stdout, "{}", format_audit_table(&bare)).ok();
    if with_issues == 0 {
        writeln!(stdout, "\n✓ All runners conform to the host-class scheme.").ok();
    } else {
        writeln!(
            stdout,
            "\n⚠︎ {with_issues} runner(s) drift from the host-class scheme \
             (<repo>-<class>-NN + <repo>-build / <repo>-build-<class>).\n  \
             Fix labels with `shipyard runner register --labels …` or re-tag/re-register \
             the host; physical host class is confirmed by `shipyard runner capacity`."
        )
        .ok();
    }
    Ok(exit)
}

// ---------- remove ----------

/// `shipyard runner remove`.
#[allow(clippy::too_many_arguments)]
pub(super) fn remove_command<W: Write>(
    cwd: &Path,
    actions: &GitHubActions,
    name: String,
    repo: Option<String>,
    purge_dir: bool,
    yes: bool,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    if !yes {
        return Err(CliFailure::new(
            2,
            format!("Refusing to remove `{name}` without confirmation. Re-run with --yes."),
        ));
    }
    let slug = resolve_repo_slug(repo, cwd)?;
    let dir = home_dir().join(format!("actions-runner-{name}"));
    if !dir.join("config.sh").exists() {
        return Err(CliFailure::new(
            1,
            format!("no configured runner dir at {}", dir.display()),
        ));
    }

    let token = actions
        .run_gh(&[
            "api".to_owned(),
            "-X".to_owned(),
            "POST".to_owned(),
            format!("repos/{slug}/actions/runners/remove-token"),
            "--jq".to_owned(),
            ".token".to_owned(),
        ])
        .map_err(|e| CliFailure::new(2, format!("failed to get removal token: {e}")))?
        .trim()
        .to_owned();

    // Stop the service first; ignore failure (it may already be stopped).
    let _ = Command::new("./svc.sh")
        .current_dir(&dir)
        .arg("stop")
        .status();
    run_in(
        &dir,
        "./config.sh",
        &["remove", "--token", &token],
        "deregister runner",
    )?;

    if purge_dir {
        fs::remove_dir_all(&dir)
            .map_err(|e| CliFailure::new(1, format!("failed to purge runner dir: {e}")))?;
    }

    if json {
        let mut data = BTreeMap::new();
        data.insert("removed".to_owned(), Value::from(name));
        data.insert("repo".to_owned(), Value::from(slug));
        data.insert("purged_dir".to_owned(), Value::from(purge_dir));
        envelope(stdout, "runner.remove", data)?;
    } else {
        writeln!(stdout, "Removed runner `{name}` from {slug}").ok();
        if purge_dir {
            writeln!(stdout, "  purged {}", dir.display()).ok();
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---------- shared shell helpers ----------

fn run(program: &str, args: &[&str], what: &str) -> Result<(), CliFailure> {
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| CliFailure::new(1, format!("failed to {what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{what} failed (exit {:?})", status.code()),
        ))
    }
}

fn run_in(dir: &Path, program: &str, args: &[&str], what: &str) -> Result<(), CliFailure> {
    let status = Command::new(program)
        .current_dir(dir)
        .args(args)
        .status()
        .map_err(|e| CliFailure::new(1, format!("failed to {what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(CliFailure::new(
            1,
            format!("{what} failed (exit {:?})", status.code()),
        ))
    }
}

fn envelope<W: Write>(
    stdout: &mut W,
    command: &str,
    data: BTreeMap<String, Value>,
) -> Result<(), CliFailure> {
    write_json_envelope(stdout, command, data)
        .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_runner(name: &str, status: &str, busy: bool) -> ApiRunner {
        ApiRunner {
            name: name.to_owned(),
            status: status.to_owned(),
            busy,
            labels: Vec::new(),
        }
    }

    #[test]
    fn parse_dot_runner_handles_bom_and_extracts_slug() {
        let raw = "\u{feff}{\"agentName\":\"pulp-m1-01\",\"gitHubUrl\":\"https://github.com/danielraffel/pulp\"}";
        let (name, slug) = parse_dot_runner(raw).expect("parse");
        assert_eq!(name, "pulp-m1-01");
        assert_eq!(slug, "danielraffel/pulp");
    }

    #[test]
    fn parse_dot_runner_rejects_missing_fields() {
        assert!(parse_dot_runner("{}").is_none());
        assert!(parse_dot_runner("not json").is_none());
    }

    #[test]
    fn tag_command_uses_resolved_state_directory() {
        let temp = tempfile::tempdir().expect("temp");
        let state_dir = temp.path().join("override-state");
        let mut output = Vec::new();
        tag_command(&state_dir, Some("studio".to_owned()), false, &mut output).expect("set tag");
        assert_eq!(
            std::fs::read_to_string(state_dir.join("machine-tag")).expect("tag"),
            "studio\n"
        );
    }

    #[test]
    fn sha256_verification_rejects_changed_downloads() {
        let temp = tempfile::tempdir().expect("temp");
        let package = temp.path().join("package");
        std::fs::write(&package, b"known bytes").expect("write package");
        let expected = hex::encode(Sha256::digest(b"known bytes"));
        verify_sha256(&package, &expected, "test package").expect("matching digest");
        assert!(verify_sha256(&package, "00", "test package").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn installed_runner_version_reads_the_binary_not_directory_presence() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let listener = temp.path().join("bin/Runner.Listener");
        std::fs::create_dir_all(listener.parent().expect("parent")).expect("bin dir");
        std::fs::write(&listener, "#!/bin/sh\nprintf '2.334.0\\n'\n").expect("listener");
        let mut permissions = std::fs::metadata(&listener)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&listener, permissions).expect("chmod");
        assert_eq!(
            installed_runner_version(temp.path()).as_deref(),
            Some("2.334.0")
        );
        assert_ne!(
            installed_runner_version(temp.path()).as_deref(),
            Some(PINNED_RUNNER_VERSION)
        );
    }

    #[test]
    fn service_installed_runner_is_reported_deferred_without_control() {
        let temp = tempfile::tempdir().expect("temp");
        let runner_dir = temp.path().join("actions-runner-pulp-m5-01");
        std::fs::create_dir_all(&runner_dir).expect("runner dir");
        std::fs::write(runner_dir.join(".runner"), "{}\n").expect("runner marker");
        std::fs::write(runner_dir.join(".service"), "plist\n").expect("service marker");
        let plan = vec![RunnerPlan {
            name: "pulp-m5-01".to_owned(),
            dir: runner_dir,
            work: temp.path().join("work"),
            registered: Some(api_runner("pulp-m5-01", "online", false)),
            parallel: 4,
        }];
        let deferred = BTreeMap::from([("pulp-m5-01".to_owned(), "service_installed".to_owned())]);
        let mut output = Vec::new();
        report_register(
            &mut output,
            true,
            "Generous-Corp/pulp",
            "m5",
            "self-hosted,macos,arm64",
            temp.path(),
            4,
            &plan,
            &deferred,
            &BTreeMap::new(),
            true,
        )
        .expect("report");
        let json: Value = serde_json::from_slice(&output).expect("one JSON document");
        assert_eq!(
            json.pointer("/runners/0/action").and_then(Value::as_str),
            Some("defer_service_installed")
        );

        let mut post_apply = Vec::new();
        report_register(
            &mut post_apply,
            true,
            "Generous-Corp/pulp",
            "m5",
            "self-hosted,macos,arm64",
            temp.path(),
            4,
            &plan,
            &BTreeMap::new(),
            &BTreeMap::new(),
            false,
        )
        .expect("post-apply report");
        let post_apply: Value =
            serde_json::from_slice(&post_apply).expect("one post-apply JSON document");
        assert_eq!(
            post_apply
                .pointer("/runners/0/action")
                .and_then(Value::as_str),
            Some("upgrade_or_refresh")
        );

        let mut retained_output = Vec::new();
        let retained = BTreeMap::from([("pulp-m5-01".to_owned(), "pinned_service".to_owned())]);
        report_register(
            &mut retained_output,
            true,
            "Generous-Corp/pulp",
            "m5",
            "self-hosted,macos,arm64",
            temp.path(),
            4,
            &plan,
            &BTreeMap::new(),
            &retained,
            true,
        )
        .expect("retained report");
        let retained_output: Value =
            serde_json::from_slice(&retained_output).expect("one retained JSON document");
        assert_eq!(
            retained_output
                .pointer("/runners/0/action")
                .and_then(Value::as_str),
            Some("retain_pinned_service")
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_service_less_activation_uninstalls_then_restores_original() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let runner = temp.path().join("actions-runner-pulp-m5-01");
        let staged = temp
            .path()
            .join(".actions-runner-pulp-m5-01.shipyard-stage");
        std::fs::create_dir_all(&runner).expect("runner");
        std::fs::write(runner.join("original-marker"), "intact\n").expect("marker");
        std::fs::create_dir_all(staged.join("bin")).expect("staged bin");
        std::fs::write(staged.join("bin/Runner.Listener"), "new\n").expect("new listener");
        let staged_service = staged.join("svc.sh");
        std::fs::write(
            &staged_service,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" >> ../service-recovery\ncase \"$1\" in\n  start) touch ../replacement-running; exit 1 ;;\n  uninstall) rm -f ../replacement-running ;;\nesac\n",
        )
        .expect("staged service");
        let mut permissions = std::fs::metadata(&staged_service)
            .expect("metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&staged_service, permissions).expect("chmod");

        let returned = activate_staged_service_install(&runner, &staged)
            .expect_err("failed start must restore original");
        assert!(returned.message().contains("start runner service"));
        assert_eq!(
            std::fs::read_to_string(runner.join("original-marker")).expect("original restored"),
            "intact\n"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("service-recovery")).expect("invocations"),
            "install\nstart\nuninstall\n"
        );
        assert!(!temp.path().join("replacement-running").exists());
        assert!(
            !temp
                .path()
                .join(".actions-runner-pulp-m5-01.shipyard-backup")
                .exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_staged_extraction_never_stops_or_changes_the_live_runner() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp");
        let runner = temp.path().join("actions-runner-pulp-m5-01");
        let package_root = temp.path().join("package-root");
        std::fs::create_dir_all(runner.join("bin")).expect("runner bin");
        std::fs::create_dir_all(package_root.join("bin")).expect("package bin");
        let listener = runner.join("bin/Runner.Listener");
        std::fs::write(&listener, "#!/bin/sh\nprintf '2.334.0\\n'\n").expect("old listener");
        let service = runner.join("svc.sh");
        std::fs::write(
            &service,
            "#!/bin/sh\nprintf '%s\\n' \"$1\" > ../service-invocation\n",
        )
        .expect("service");
        let corrupt = package_root.join("bin/Runner.Listener");
        std::fs::write(&corrupt, "#!/bin/sh\nprintf 'corrupt\\n'\n").expect("corrupt");
        for executable in [&listener, &service, &corrupt] {
            let mut permissions = std::fs::metadata(executable)
                .expect("metadata")
                .permissions();
            permissions.set_mode(0o700);
            std::fs::set_permissions(executable, permissions).expect("chmod");
        }
        let package = temp.path().join("runner.tar.gz");
        let status = Command::new("/usr/bin/tar")
            .args(["czf", package.to_str().expect("package path"), "-C"])
            .arg(&package_root)
            .arg(".")
            .status()
            .expect("create package");
        assert!(status.success());

        let error = prepare_staged_runner(
            &runner,
            &package,
            RunnerInstallation {
                configured: true,
                service_installed: false,
            },
            &temp.path().join("ci"),
            &temp.path().join("work"),
            4,
        )
        .expect_err("corrupt staged listener must fail before service stop");
        assert!(error.message().contains("staged runner is not pinned"));
        assert_eq!(
            installed_runner_version(&runner).as_deref(),
            Some("2.334.0")
        );
        assert!(!temp.path().join("service-invocation").exists());
        assert!(
            !temp
                .path()
                .join(".actions-runner-pulp-m5-01.shipyard-stage")
                .exists()
        );
    }

    #[test]
    fn runner_installation_state_distinguishes_upgrade_from_fresh_registration() {
        let temp = tempfile::tempdir().expect("temp");
        assert_eq!(
            inspect_runner_installation(temp.path()),
            RunnerInstallation {
                configured: false,
                service_installed: false,
            }
        );

        std::fs::write(temp.path().join(".runner"), "{}\n").expect("runner config");
        std::fs::write(temp.path().join(".service"), "plist\n").expect("service marker");
        assert_eq!(
            inspect_runner_installation(temp.path()),
            RunnerInstallation {
                configured: true,
                service_installed: true,
            }
        );
    }

    #[test]
    fn service_appearing_after_preflight_blocks_the_mutation_boundary() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join(".runner"), "{}\n").expect("runner config");
        require_service_less_at_boundary(temp.path(), "pulp-m5-01", "before staging")
            .expect("service-less runner");

        std::fs::write(temp.path().join(".service"), "plist\n").expect("service marker");
        let error = require_service_less_at_boundary(temp.path(), "pulp-m5-01", "during staging")
            .expect_err("new service must transfer control away from reconciliation");
        assert_eq!(error.code, 3);
        assert!(error.message().contains("during staging"));
    }

    #[test]
    fn unconfigured_existing_directory_is_not_a_fresh_install_target() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("partial-file"), "partial\n").expect("partial");
        let installation = inspect_runner_installation(temp.path());
        assert!(!installation.configured);
        let error = validate_installation_shape(temp.path(), installation)
            .expect_err("partial directory must not be overwritten as a fresh install");
        assert!(error.message().contains("unconfigured runner directory"));
    }

    #[test]
    fn configured_runner_must_still_exist_in_github_inventory() {
        let temp = tempfile::tempdir().expect("temp");
        let installation = RunnerInstallation {
            configured: true,
            service_installed: true,
        };
        std::fs::write(
            temp.path().join(".runner"),
            r#"{"agentName":"pulp-studio-03","gitHubUrl":"https://github.com/Generous-Corp/pulp"}"#,
        )
        .expect("runner identity");

        let error = validate_planned_runner_identity(
            temp.path(),
            "pulp-studio-03",
            "Generous-Corp/pulp",
            installation,
            None,
        )
        .expect_err("orphaned local runner config must fail closed");
        assert!(error.message.contains("not registered in GitHub"));

        let registered = api_runner("pulp-studio-03", "online", false);
        validate_planned_runner_identity(
            temp.path(),
            "pulp-studio-03",
            "Generous-Corp/pulp",
            installation,
            Some(&registered),
        )
        .expect("matching server inventory");
    }

    #[test]
    fn configured_runner_identity_must_match_name_and_repository() {
        let temp = tempfile::tempdir().expect("temp");
        let installation = RunnerInstallation {
            configured: true,
            service_installed: true,
        };
        let registered = api_runner("pulp-m5-01", "online", false);
        std::fs::write(
            temp.path().join(".runner"),
            r#"{"agentName":"forge-m5-01","gitHubUrl":"https://github.com/Generous-Corp/forge"}"#,
        )
        .expect("runner identity");
        let error = validate_planned_runner_identity(
            temp.path(),
            "pulp-m5-01",
            "Generous-Corp/pulp",
            installation,
            Some(&registered),
        )
        .expect_err("foreign runner must never be controlled");
        assert!(error.message().contains("identity mismatch"));

        std::fs::write(
            temp.path().join(".runner"),
            r#"{"agentName":"pulp-m5-01","gitHubUrl":"https://github.com/Generous-Corp/forge"}"#,
        )
        .expect("runner identity");
        let error = validate_planned_runner_identity(
            temp.path(),
            "pulp-m5-01",
            "Generous-Corp/pulp",
            installation,
            Some(&registered),
        )
        .expect_err("foreign repository must never be controlled");
        assert!(error.message().contains("repository mismatch"));
    }

    #[test]
    fn service_less_runner_requires_offline_idle_evidence() {
        let runners = vec![api_runner("pulp-m5-01", "online", true)];
        let error = require_offline_idle_runner(&runners, "pulp-m5-01")
            .expect_err("busy runner must never be stopped");
        assert_eq!(error.code, 3);
        assert!(error.message().contains("online/busy"));

        let unknown = vec![api_runner("pulp-m5-01", "", false)];
        let error = require_offline_idle_runner(&unknown, "pulp-m5-01")
            .expect_err("missing status evidence must fail closed");
        assert_eq!(error.code, 1);
        assert!(error.message().contains("unknown GitHub status"));

        let online = vec![api_runner("pulp-m5-01", "online", false)];
        let error = require_offline_idle_runner(&online, "pulp-m5-01")
            .expect_err("online service-less runner may be manually active");
        assert_eq!(error.code, 3);
        assert!(error.message().contains("online without"));

        let offline = vec![api_runner("pulp-m5-01", "offline", false)];
        require_offline_idle_runner(&offline, "pulp-m5-01")
            .expect("offline idle service-less runner is safe to stage");
    }

    #[test]
    fn registration_plan_upgrades_local_registered_runners_before_adding_capacity() {
        let temp = tempfile::tempdir().expect("temp");
        let ci_root = temp.path().join("ci");
        for name in ["pulp-m5-01", "pulp-m5-02"] {
            let dir = temp.path().join(format!("actions-runner-{name}"));
            std::fs::create_dir_all(&dir).expect("runner dir");
            std::fs::write(dir.join(".runner"), "{}\n").expect("runner config");
        }
        let registered = vec![
            api_runner("pulp-m5-01", "online", false),
            api_runner("pulp-m5-02", "offline", false),
            api_runner("pulp-m1-01", "online", false),
        ];

        let existing_plan = build_runner_plan(&registered, "pulp", "m5", 1, &ci_root, temp.path());
        assert_eq!(
            existing_plan
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["pulp-m5-01", "pulp-m5-02", "pulp-m5-03"]
        );

        let expanded_plan = build_runner_plan(&registered, "pulp", "m5", 3, &ci_root, temp.path());
        assert_eq!(
            expanded_plan
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "pulp-m5-01",
                "pulp-m5-02",
                "pulp-m5-03",
                "pulp-m5-04",
                "pulp-m5-05"
            ]
        );
        assert_eq!(expanded_plan[2].work, ci_root.join("work/pulp-m5-03"));
        assert_eq!(parallel_per_runner(26, expanded_plan.len()), 5);
    }

    #[test]
    fn deferred_allocations_are_reserved_before_new_parallelism_is_assigned() {
        let temp = tempfile::tempdir().expect("temp");
        let mut plan = Vec::new();
        let mut deferred = BTreeMap::new();
        for (name, existing_parallel) in [
            ("pulp-m5-01", Some(2)),
            ("pulp-m5-02", Some(2)),
            ("pulp-m5-03", None),
            ("pulp-m5-04", None),
        ] {
            let dir = temp.path().join(name);
            std::fs::create_dir_all(&dir).expect("runner dir");
            if let Some(value) = existing_parallel {
                std::fs::write(
                    dir.join(".env"),
                    format!("CMAKE_BUILD_PARALLEL_LEVEL={value}\n"),
                )
                .expect("runner env");
                deferred.insert(name.to_owned(), "service_installed".to_owned());
            }
            plan.push(RunnerPlan {
                name: name.to_owned(),
                dir,
                work: temp.path().join(format!("work-{name}")),
                registered: None,
                parallel: 0,
            });
        }

        let eligible_parallel =
            allocate_plan_parallel(&mut plan, &deferred, 12).expect("capacity plan");
        assert_eq!(eligible_parallel, 4);
        assert_eq!(
            plan.iter().map(|entry| entry.parallel).collect::<Vec<_>>(),
            vec![2, 2, 4, 4]
        );

        for entry in &plan[..2] {
            std::fs::write(entry.dir.join(".env"), "CMAKE_BUILD_PARALLEL_LEVEL=6\n")
                .expect("runner env");
        }
        let error = allocate_plan_parallel(&mut plan, &deferred, 12)
            .expect_err("fully reserved host must not overcommit new runners");
        assert_eq!(error.code, 3);
        assert!(error.message().contains("leaving 0 for 2 eligible"));
    }

    #[test]
    fn cross_repo_and_old_tag_local_runners_reserve_host_capacity() {
        let temp = tempfile::tempdir().expect("temp");
        for (name, parallel) in [
            ("actions-runner-forge-m5-01", 3),
            ("actions-runner-pulp-oldtag-01", 2),
        ] {
            let dir = temp.path().join(name);
            std::fs::create_dir_all(&dir).expect("runner dir");
            std::fs::write(dir.join(".runner"), "{}\n").expect("runner config");
            std::fs::write(
                dir.join(".env"),
                format!("CMAKE_BUILD_PARALLEL_LEVEL={parallel}\n"),
            )
            .expect("runner env");
        }
        let planned_dir = temp.path().join("actions-runner-pulp-m5-01");
        std::fs::create_dir_all(&planned_dir).expect("planned runner dir");
        std::fs::write(planned_dir.join(".runner"), "{}\n").expect("planned config");
        std::fs::write(planned_dir.join(".env"), "CMAKE_BUILD_PARALLEL_LEVEL=7\n")
            .expect("planned env");
        let plan = vec![RunnerPlan {
            name: "pulp-m5-01".to_owned(),
            dir: planned_dir,
            work: temp.path().join("work"),
            registered: Some(api_runner("pulp-m5-01", "online", false)),
            parallel: 0,
        }];

        assert_eq!(
            external_runner_parallel(temp.path(), &plan).expect("capacity"),
            5
        );
    }

    // Runner provisioning targets self-hosted macOS runners and the env file's
    // cache paths are built with `Path::join`, so the forward-slash assertions
    // only hold on Unix. On a Windows CI build host the separators are `\`,
    // which is a false failure — gate the path-format assertion to Unix.
    #[cfg(unix)]
    #[test]
    fn runner_env_file_points_at_shared_caches() {
        let env = runner_env_file(
            Path::new("/Volumes/Workshop/ci/pulp"),
            Path::new("/Volumes/Workshop/ci/pulp/work/pulp-studio-01"),
            Path::new("/Users/me/actions-runner-pulp-studio-01"),
            9,
        );
        assert!(env.contains("CCACHE_DIR=/Volumes/Workshop/ci/pulp/cache/ccache"));
        assert!(env.contains("CCACHE_BASEDIR=/Volumes/Workshop/ci/pulp/work/pulp-studio-01"));
        assert!(
            env.contains("FETCHCONTENT_BASE_DIR=/Volumes/Workshop/ci/pulp/cache/fetchcontent-src")
        );
        assert!(env.contains("CMAKE_BUILD_PARALLEL_LEVEL=9"));
        assert!(env.contains("CTEST_PARALLEL_LEVEL=9"));
        assert!(env.contains("CCACHE_NOHASHDIR=true"));
        assert!(env.contains("CCACHE_NODEPEND=true"));
        assert!(env.contains("CCACHE_COMPILERCHECK=content"));
        assert!(!env.contains("CCACHE_DEPEND=true"));
        assert!(
            env.contains("RUSTUP_HOME=/Users/me/actions-runner-pulp-studio-01/_toolcache/rustup")
        );
        assert!(
            env.contains("CARGO_HOME=/Users/me/actions-runner-pulp-studio-01/_toolcache/cargo")
        );
    }

    #[cfg(unix)]
    #[test]
    fn runner_path_file_is_system_first_and_runner_private() {
        let path = runner_path_file(Path::new("/Users/me/actions-runner-pulp-studio-01"));
        assert!(path.starts_with("/usr/bin:/bin:/usr/sbin:/sbin:"));
        assert!(path.contains(":/Users/me/actions-runner-pulp-studio-01/_toolcache/cargo/bin:"));
        assert!(path.find("/usr/bin").unwrap() < path.find("/opt/homebrew/bin").unwrap());
    }

    #[test]
    fn runner_package_and_registration_are_pinned_without_auto_update() {
        let (name, url) = runner_package_url();
        assert_eq!(name, "actions-runner-osx-arm64-2.335.1.tar.gz");
        assert!(url.contains("/download/v2.335.1/"));
        assert!(!url.contains("latest"));
        assert_eq!(
            PINNED_RUNNER_SHA256,
            "e1a9bc7a3661e06fa0b129d15c2064fe65dc81a431001d8958a9db1409b73769"
        );

        let entry = RunnerPlan {
            name: "Shipyard-studio-02".to_owned(),
            dir: PathBuf::from("/Users/me/actions-ci/Shipyard-studio-02"),
            work: PathBuf::from("/Volumes/Workshop/ci/shipyard/work/Shipyard-studio-02"),
            registered: None,
            parallel: 4,
        };
        let args = runner_config_args("danielraffel/Shipyard", "secret", &entry, "local-mac");
        assert!(args.iter().any(|arg| arg == "--disableupdate"));
        assert!(
            args.windows(2)
                .any(|pair| pair == ["--name", "Shipyard-studio-02"])
        );
    }
}
