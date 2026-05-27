use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use serde_json::Value;

use super::{CliFailure, cli::PinCommand};
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;
use crate::output::write_json_envelope;
use crate::pin::{
    ConsumerPin, current_global_shipyard_version, detect_consumer_pin, is_shipyard_repo,
    latest_shipyard_release_from_cwd, main_pinned_version_at_origin, normalize_target_version,
    origin_main_satisfies_target, parse_version_tuple, read_pinned_version, rewrite_pinned_version,
    would_downgrade_installed,
};

pub(super) fn pin_command<W: Write>(
    command: PinCommand,
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        PinCommand::Show => pin_show(mode, cwd, json, stdout),
        PinCommand::Bump {
            target,
            no_pr,
            skip_verify,
            allow_downgrade,
            allow_redundant,
        } => pin_bump_with_mode(
            cwd,
            mode,
            PinBumpOptions {
                target,
                pr_mode: if no_pr { PrMode::NoPr } else { PrMode::OpenPr },
                verify_mode: if skip_verify {
                    VerifyMode::Skip
                } else {
                    VerifyMode::InstallAndCheck
                },
                downgrade_policy: if allow_downgrade {
                    DowngradePolicy::Allow
                } else {
                    DowngradePolicy::Refuse
                },
                redundant_policy: if allow_redundant {
                    RedundantPolicy::Allow
                } else {
                    RedundantPolicy::Refuse
                },
            },
            json,
            stdout,
        ),
    }
}

struct PinBumpOptions {
    target: Option<String>,
    pr_mode: PrMode,
    verify_mode: VerifyMode,
    downgrade_policy: DowngradePolicy,
    redundant_policy: RedundantPolicy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PrMode {
    OpenPr,
    NoPr,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum VerifyMode {
    InstallAndCheck,
    Skip,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DowngradePolicy {
    Refuse,
    Allow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RedundantPolicy {
    Refuse,
    Allow,
}

fn pin_show<W: Write>(
    mode: RuntimeMode,
    cwd: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    pin_show_with_latest(cwd, json, stdout, || {
        latest_shipyard_release_from_cwd(mode, cwd)
    })
}

fn pin_show_with_latest<W, F>(
    cwd: &Path,
    json: bool,
    stdout: &mut W,
    latest_release: F,
) -> Result<ExitCode, CliFailure>
where
    W: Write,
    F: FnOnce() -> Option<String>,
{
    let pin = resolve_consumer_pin(cwd, "`shipyard pin` requires a consumer repo")?;
    let current = read_pinned_version(&pin.pin_file).unwrap_or_else(|| "<unknown>".to_owned());
    let latest = latest_release().unwrap_or_else(|| "<unknown - gh unavailable>".to_owned());
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("show"));
        data.insert(
            "pin_file".to_owned(),
            Value::from(pin.pin_file.display().to_string()),
        );
        data.insert("current".to_owned(), Value::from(current.clone()));
        data.insert("latest".to_owned(), Value::from(latest.clone()));
        data.insert("up_to_date".to_owned(), Value::Bool(current == latest));
        write_json_envelope(stdout, "pin", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        return Ok(ExitCode::SUCCESS);
    }

    writeln!(stdout, "pin file:  {}", pin.pin_file.display())
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "current:   {current}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    writeln!(stdout, "latest:    {latest}")
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    if current == latest {
        writeln!(stdout, "Up to date.").map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "New version available. Run `shipyard pin bump --to {latest}`."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
fn pin_bump<W: Write>(
    cwd: &Path,
    options: PinBumpOptions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    pin_bump_with_mode(cwd, RuntimeMode::Shipyard, options, json, stdout)
}

fn pin_bump_with_mode<W: Write>(
    cwd: &Path,
    mode: RuntimeMode,
    options: PinBumpOptions,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let pin = resolve_consumer_pin(cwd, "`shipyard pin bump` requires a consumer repo")?;
    ensure_pin_files_clean(&pin.repo_root)?;
    let current = read_pinned_version(&pin.pin_file);
    let target = resolve_target_with_cwd(options.target, mode, cwd)?;
    if current.as_deref() == Some(&target) {
        render_pin_noop(stdout, json, current.as_deref(), &target)?;
        return Ok(ExitCode::SUCCESS);
    }
    guard_no_global_downgrade(&target, options.downgrade_policy)?;
    guard_not_redundant(&pin.repo_root, &target, options.redundant_policy)?;

    if !json {
        writeln!(
            stdout,
            "Bumping pin: {} -> {target}",
            current.as_deref().unwrap_or("<unknown>")
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    rewrite_pinned_version(&pin.pin_file, &target)
        .map_err(|error| CliFailure::new(1, error.to_string()))?;

    if options.verify_mode == VerifyMode::InstallAndCheck {
        run_install_and_verify(&pin.repo_root, &target)?;
    }

    if options.pr_mode == PrMode::NoPr {
        render_pin_bump(
            stdout,
            json,
            "edited",
            None,
            &pin,
            current.as_deref(),
            &target,
        )?;
        return Ok(ExitCode::SUCCESS);
    }

    let pr_url = commit_push_and_open_pr(mode, &pin.repo_root, current.as_deref(), &target)?;
    render_pin_bump(
        stdout,
        json,
        "pr-opened",
        Some(&pr_url),
        &pin,
        current.as_deref(),
        &target,
    )?;
    Ok(ExitCode::SUCCESS)
}

fn resolve_consumer_pin(cwd: &Path, missing_message: &str) -> Result<ConsumerPin, CliFailure> {
    if let Some(pin) = detect_consumer_pin(cwd) {
        return Ok(pin);
    }
    if is_shipyard_repo(cwd) {
        return Err(CliFailure::new(
            1,
            "Refusing: this is the Shipyard repo. `shipyard pin` runs in consumer repos that pin Shipyard, not in Shipyard itself.",
        ));
    }
    Err(CliFailure::new(
        1,
        format!("No tools/shipyard.toml found. {missing_message}."),
    ))
}

fn ensure_pin_files_clean(repo_root: &Path) -> Result<(), CliFailure> {
    let output = Command::new("git")
        .args([
            "status",
            "--porcelain",
            "--",
            "tools/shipyard.toml",
            "tools/install-shipyard.sh",
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|error| CliFailure::new(1, format!("git status failed: {error}")))?;
    if output.status.success() && !output.stdout.is_empty() {
        return Err(CliFailure::new(
            1,
            format!(
                "Refusing: pin files are already modified:\n{}Commit or stash them first - pin bump folds into a PR.",
                String::from_utf8_lossy(&output.stdout)
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
fn resolve_target(target: Option<String>) -> Result<String, CliFailure> {
    resolve_target_with_cwd(target, RuntimeMode::Shipyard, Path::new("."))
}

fn resolve_target_with_cwd(
    target: Option<String>,
    mode: RuntimeMode,
    cwd: &Path,
) -> Result<String, CliFailure> {
    match target {
        Some(target) => Ok(normalize_target_version(&target)),
        None => latest_shipyard_release_from_cwd(mode, cwd)
            .map(|target| normalize_target_version(&target))
            .ok_or_else(|| {
                CliFailure::new(
                    1,
                    "Could not resolve latest Shipyard release. Pass --to vX.Y.Z explicitly, or check `gh auth status`.",
                )
            }),
    }
}

fn guard_no_global_downgrade(
    target: &str,
    downgrade_policy: DowngradePolicy,
) -> Result<(), CliFailure> {
    guard_no_global_downgrade_with_installed(
        target,
        downgrade_policy,
        current_global_shipyard_version,
    )
}

fn guard_no_global_downgrade_with_installed<F>(
    target: &str,
    downgrade_policy: DowngradePolicy,
    installed_version: F,
) -> Result<(), CliFailure>
where
    F: FnOnce() -> Option<String>,
{
    if downgrade_policy == DowngradePolicy::Allow {
        return Ok(());
    }
    if parse_version_tuple(target).is_none() {
        return Ok(());
    }
    let Some(installed) = installed_version() else {
        return Ok(());
    };
    if would_downgrade_installed(target, &installed) {
        return Err(CliFailure::new(
            1,
            format!(
                "Refusing: target {target} is older than the currently-installed shipyard binary (v{installed}). Running ./tools/install-shipyard.sh would DOWNGRADE your global binary.\nLikely cause: this worktree is stale - rebase onto main before bumping, or pass --allow-downgrade to proceed anyway."
            ),
        ));
    }
    Ok(())
}

fn guard_not_redundant(
    repo_root: &Path,
    target: &str,
    redundant_policy: RedundantPolicy,
) -> Result<(), CliFailure> {
    if redundant_policy == RedundantPolicy::Allow {
        return Ok(());
    }
    if parse_version_tuple(target).is_none() {
        return Ok(());
    }
    let Some(main_pin) = main_pinned_version_at_origin(repo_root) else {
        return Ok(());
    };
    if origin_main_satisfies_target(&main_pin, target) {
        return Err(CliFailure::new(
            1,
            format!(
                "Refusing: origin/main already pins {main_pin}, which is >= target {target}. Opening a PR here would be redundant or regressive.\nLikely cause: this worktree is behind main - rebase or merge origin/main first, or pass --allow-redundant to proceed anyway."
            ),
        ));
    }
    Ok(())
}

fn run_install_and_verify(repo_root: &Path, target: &str) -> Result<(), CliFailure> {
    let install_script = repo_root.join("tools").join("install-shipyard.sh");
    if !install_script.exists() {
        return Err(CliFailure::new(
            1,
            format!(
                "Expected installer at {} - consumer repos must ship a tools/install-shipyard.sh wrapper.",
                install_script.display()
            ),
        ));
    }
    let status = Command::new("bash")
        .arg(&install_script)
        .current_dir(repo_root)
        .status()
        .map_err(|error| CliFailure::new(1, format!("install-shipyard.sh failed: {error}")))?;
    if !status.success() {
        return Err(CliFailure::new(
            status.code().unwrap_or(1).try_into().unwrap_or(1),
            format!(
                "install-shipyard.sh exited {}. Pin edit left in place for inspection; not opening PR.",
                status.code().unwrap_or(1)
            ),
        ));
    }
    let output = Command::new("shipyard")
        .arg("--version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            CliFailure::new(1, format!("Could not invoke shipyard --version: {error}"))
        })?;
    let expected = target.trim_start_matches('v');
    if !String::from_utf8_lossy(&output.stdout).contains(expected) {
        return Err(CliFailure::new(
            1,
            format!(
                "shipyard --version reported {:?}; expected to contain {:?}.",
                String::from_utf8_lossy(&output.stdout),
                expected
            ),
        ));
    }
    Ok(())
}

fn commit_push_and_open_pr(
    mode: RuntimeMode,
    repo_root: &Path,
    current: Option<&str>,
    target: &str,
) -> Result<String, CliFailure> {
    let branch = format!("chore/bump-shipyard-pin-to-{target}");
    run_checked(repo_root, "git", &["checkout", "-b", &branch])?;
    run_checked(repo_root, "git", &["add", "tools/shipyard.toml"])?;
    let commit_msg = format!(
        "chore: bump Shipyard pin {} -> {target}\n\nSee https://github.com/danielraffel/Shipyard/releases/tag/{target} for release notes.",
        current.unwrap_or("unknown")
    );
    run_checked(repo_root, "git", &["commit", "-m", &commit_msg])?;
    run_checked(repo_root, "git", &["push", "-u", "origin", &branch])?;

    let body = format!(
        "Bumps the pinned Shipyard version from {} to **{target}**.\n\nVerified locally:\n- [x] `./tools/install-shipyard.sh` succeeded\n- [x] `shipyard --version` matches target\n\nRelease notes: https://github.com/danielraffel/Shipyard/releases/tag/{target}",
        current.unwrap_or("unknown")
    );
    let output = GhClient::from_cwd(mode, repo_root)
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .prepare_command(
            repo_root,
            None,
            GhSupervision::Unsupervised,
            GhAuthPolicy::Default,
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?
        .args([
            "pr",
            "create",
            "--title",
            &format!(
                "chore: bump Shipyard pin {} -> {target}",
                current.unwrap_or("?")
            ),
            "--body",
            &body,
        ])
        .output()
        .map_err(|error| CliFailure::new(1, format!("gh pr create failed: {error}")))?;
    if !output.status.success() {
        return Err(CliFailure::new(
            1,
            format!(
                "gh pr create failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .last()
        .unwrap_or_default()
        .trim()
        .to_owned())
}

fn run_checked(repo_root: &Path, command: &str, args: &[&str]) -> Result<(), CliFailure> {
    let output = Command::new(command)
        .args(args)
        .current_dir(repo_root)
        .output()
        .map_err(|error| CliFailure::new(1, format!("{command} failed: {error}")))?;
    if output.status.success() {
        Ok(())
    } else {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut message = format!("{command} {} exited with {}", args.join(" "), output.status);
        if !stdout.trim().is_empty() {
            write!(message, "\nstdout:\n{stdout}").expect("write to string");
        }
        if !stderr.trim().is_empty() {
            write!(message, "\nstderr:\n{stderr}").expect("write to string");
        }
        Err(CliFailure::new(1, message))
    }
}

fn render_pin_noop<W: Write>(
    stdout: &mut W,
    json: bool,
    current: Option<&str>,
    target: &str,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("bump"));
        data.insert("result".to_owned(), Value::from("noop"));
        data.insert(
            "current".to_owned(),
            current.map_or(Value::Null, Value::from),
        );
        data.insert("target".to_owned(), Value::from(target));
        write_json_envelope(stdout, "pin", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(stdout, "Already pinned to {target} - nothing to do.")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

fn render_pin_bump<W: Write>(
    stdout: &mut W,
    json: bool,
    result: &str,
    pr_url: Option<&str>,
    pin: &ConsumerPin,
    current: Option<&str>,
    target: &str,
) -> Result<(), CliFailure> {
    if json {
        let mut data = BTreeMap::new();
        data.insert("event".to_owned(), Value::from("bump"));
        data.insert("result".to_owned(), Value::from(result));
        data.insert(
            "pin_file".to_owned(),
            Value::from(pin.pin_file.display().to_string()),
        );
        data.insert(
            "repo_root".to_owned(),
            Value::from(pin.repo_root.display().to_string()),
        );
        data.insert("from".to_owned(), current.map_or(Value::Null, Value::from));
        data.insert("to".to_owned(), Value::from(target));
        if let Some(pr_url) = pr_url {
            data.insert("pr_url".to_owned(), Value::from(pr_url));
        }
        write_json_envelope(stdout, "pin", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else if let Some(pr_url) = pr_url {
        writeln!(stdout, "PR opened: {pr_url}")
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        writeln!(
            stdout,
            "--no-pr: edit left in the working tree. Commit + PR yourself when ready."
        )
        .map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::{Command, ExitCode};

    use serde_json::Value;
    use tempfile::TempDir;

    use super::{
        ConsumerPin, DowngradePolicy, PinBumpOptions, PinCommand, PrMode, RedundantPolicy,
        VerifyMode, guard_no_global_downgrade_with_installed, guard_not_redundant, pin_bump,
        pin_command, pin_show_with_latest, render_pin_bump, render_pin_noop, resolve_consumer_pin,
        resolve_target, run_checked, run_install_and_verify,
    };
    use crate::identity::RuntimeMode;
    use crate::pin::read_pinned_version;

    fn write_consumer_files(repo: &Path, version: &str) {
        let tools = repo.join("tools");
        std::fs::create_dir_all(&tools).expect("tools dir");
        std::fs::write(
            tools.join("shipyard.toml"),
            format!("[shipyard]\nversion = \"{version}\"\nrepo = \"danielraffel/Shipyard\"\n"),
        )
        .expect("pin file");
        std::fs::write(tools.join("install-shipyard.sh"), "#!/bin/sh\nexit 0\n")
            .expect("install wrapper");
    }

    fn init_consumer_repo(repo: &Path, version: &str) {
        std::fs::create_dir_all(repo).expect("repo dir");
        git(repo, &["init", "--quiet", "--initial-branch=main"]);
        write_consumer_files(repo, version);
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "seed"]);
    }

    fn no_pr_options(target: &str) -> PinBumpOptions {
        PinBumpOptions {
            target: Some(target.to_owned()),
            pr_mode: PrMode::NoPr,
            verify_mode: VerifyMode::Skip,
            downgrade_policy: DowngradePolicy::Allow,
            redundant_policy: RedundantPolicy::Allow,
        }
    }

    #[test]
    fn pin_show_json_uses_injected_latest_release() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&repo, "v0.50.0");
        let mut out = Vec::new();

        let code = pin_show_with_latest(&repo, true, &mut out, || Some("v0.50.0".to_owned()))
            .expect("show should succeed");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["command"], "pin");
        assert_eq!(payload["event"], "show");
        assert_eq!(payload["current"], "v0.50.0");
        assert_eq!(payload["latest"], "v0.50.0");
        assert_eq!(payload["up_to_date"], true);
        assert!(
            payload["pin_file"]
                .as_str()
                .expect("pin_file")
                .replace('\\', "/")
                .ends_with("tools/shipyard.toml")
        );
    }

    #[test]
    fn pin_show_human_reports_available_update_and_unknown_latest() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&repo, "v0.49.0");
        let mut out = Vec::new();

        let code = pin_show_with_latest(&repo, false, &mut out, || Some("v0.50.0".to_owned()))
            .expect("show should succeed");

        let text = String::from_utf8(out).expect("utf8");
        assert_eq!(code, ExitCode::SUCCESS);
        assert!(text.contains("current:   v0.49.0"));
        assert!(text.contains("latest:    v0.50.0"));
        assert!(text.contains("shipyard pin bump --to v0.50.0"));

        let mut unavailable = Vec::new();
        pin_show_with_latest(&repo, false, &mut unavailable, || None).expect("show should succeed");
        assert!(
            String::from_utf8(unavailable)
                .expect("utf8")
                .contains("<unknown - gh unavailable>")
        );
    }

    #[test]
    fn pin_show_human_reports_up_to_date() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&repo, "v0.50.0");
        let mut out = Vec::new();

        pin_show_with_latest(&repo, false, &mut out, || Some("v0.50.0".to_owned()))
            .expect("show should succeed");

        assert!(
            String::from_utf8(out)
                .expect("utf8")
                .contains("Up to date.")
        );
    }

    #[test]
    fn pin_command_bump_no_pr_json_rewrites_from_subdirectory() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        let subdir = repo.join("nested").join("project");
        init_consumer_repo(&repo, "v0.40.0");
        std::fs::create_dir_all(&subdir).expect("subdir");
        let mut out = Vec::new();

        let code = pin_command(
            PinCommand::Bump {
                target: Some("0.99.0".to_owned()),
                no_pr: true,
                skip_verify: true,
                allow_downgrade: true,
                allow_redundant: true,
            },
            RuntimeMode::Shipyard,
            &subdir,
            true,
            &mut out,
        )
        .expect("pin bump should succeed");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(payload["command"], "pin");
        assert_eq!(payload["event"], "bump");
        assert_eq!(payload["result"], "edited");
        assert_eq!(payload["from"], "v0.40.0");
        assert_eq!(payload["to"], "v0.99.0");
        assert_eq!(
            read_pinned_version(&repo.join("tools").join("shipyard.toml")),
            Some("v0.99.0".to_owned())
        );
    }

    #[test]
    fn pin_bump_noop_returns_before_verify_or_pr() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&repo, "v0.50.0");
        let mut out = Vec::new();

        let code =
            pin_bump(&repo, no_pr_options("v0.50.0"), false, &mut out).expect("noop succeeds");

        assert_eq!(code, ExitCode::SUCCESS);
        assert_eq!(
            String::from_utf8(out).expect("utf8"),
            "Already pinned to v0.50.0 - nothing to do.\n"
        );
    }

    #[test]
    fn pin_bump_refuses_dirty_pin_files_before_rewrite() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&repo, "v0.40.0");
        std::fs::write(
            repo.join("tools").join("shipyard.toml"),
            "[shipyard]\nversion = \"v0.41.0\"\n",
        )
        .expect("dirty pin");
        let mut out = Vec::new();

        let err = pin_bump(&repo, no_pr_options("v0.42.0"), true, &mut out)
            .expect_err("dirty pin should be refused");

        assert!(out.is_empty());
        assert_eq!(err.code, 1);
        assert!(err.message.contains("pin files are already modified"));
        assert!(err.message.contains("tools/shipyard.toml"));
        assert_eq!(
            read_pinned_version(&repo.join("tools").join("shipyard.toml")),
            Some("v0.41.0".to_owned())
        );
    }

    #[test]
    fn resolve_target_normalizes_explicit_versions() {
        assert_eq!(
            resolve_target(Some("0.50.0".to_owned())).expect("target"),
            "v0.50.0"
        );
        assert_eq!(
            resolve_target(Some("v0.51.0".to_owned())).expect("target"),
            "v0.51.0"
        );
    }

    #[test]
    fn downgrade_guard_refuses_target_below_installed_version() {
        let err =
            guard_no_global_downgrade_with_installed("v0.49.0", DowngradePolicy::Refuse, || {
                Some("0.50.0".to_owned())
            })
            .expect_err("downgrade should be refused");

        assert_eq!(err.code, 1);
        assert!(err.message.contains("target v0.49.0 is older"));
        assert!(
            err.message
                .contains("currently-installed shipyard binary (v0.50.0)")
        );
        assert!(err.message.contains("--allow-downgrade"));
    }

    #[test]
    fn downgrade_guard_allows_unparseable_or_unavailable_inputs() {
        assert!(
            guard_no_global_downgrade_with_installed("latest", DowngradePolicy::Refuse, || Some(
                "0.50.0".to_owned()
            ))
            .is_ok()
        );
        assert!(
            guard_no_global_downgrade_with_installed("v0.49.0", DowngradePolicy::Refuse, || None)
                .is_ok()
        );
        assert!(
            guard_no_global_downgrade_with_installed("v0.49.0", DowngradePolicy::Allow, || Some(
                "0.50.0".to_owned()
            ))
            .is_ok()
        );
    }

    #[test]
    fn redundant_guard_allows_unparseable_or_policy_allow_inputs() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&repo, "v0.50.0");

        assert!(guard_not_redundant(&repo, "latest", RedundantPolicy::Refuse).is_ok());
        assert!(guard_not_redundant(&repo, "v0.49.0", RedundantPolicy::Allow).is_ok());
    }

    #[test]
    fn resolve_consumer_pin_refuses_shipyard_repo_context() {
        let temp = TempDir::new().expect("tempdir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname = \"shipyard\"\nversion = \"0.51.0\"\n",
        )
        .expect("cargo");

        let err = resolve_consumer_pin(temp.path(), "missing").expect_err("shipyard repo refused");

        assert_eq!(err.code, 1);
        assert!(err.message.contains("Refusing: this is the Shipyard repo"));
    }

    #[test]
    fn run_install_and_verify_reports_missing_installer() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        std::fs::create_dir_all(repo.join("tools")).expect("tools dir");

        let err = run_install_and_verify(&repo, "v0.50.0").expect_err("missing installer");

        assert_eq!(err.code, 1);
        assert!(err.message.contains("Expected installer"));
        assert!(err.message.contains("tools/install-shipyard.sh"));
    }

    #[cfg(unix)]
    #[test]
    fn run_install_and_verify_reports_installer_failure_before_version_check() {
        let temp = TempDir::new().expect("tempdir");
        let repo = temp.path().join("consumer");
        std::fs::create_dir_all(repo.join("tools")).expect("tools dir");
        std::fs::write(
            repo.join("tools").join("install-shipyard.sh"),
            "#!/bin/sh\nexit 17\n",
        )
        .expect("installer");

        let err = run_install_and_verify(&repo, "v0.50.0").expect_err("installer fails");

        assert_eq!(err.code, 17);
        assert!(err.message.contains("install-shipyard.sh exited 17"));
    }

    #[test]
    fn redundant_guard_refuses_when_origin_main_already_satisfies_target() {
        let temp = TempDir::new().expect("tempdir");
        let remote = temp.path().join("remote");
        let repo = temp.path().join("consumer");
        init_consumer_repo(&remote, "v0.50.0");
        git(
            temp.path(),
            &[
                "clone",
                "--quiet",
                remote.to_str().expect("remote"),
                "consumer",
            ],
        );

        let err = guard_not_redundant(&repo, "v0.49.0", RedundantPolicy::Refuse)
            .expect_err("redundant bump should be refused");

        assert_eq!(err.code, 1);
        assert!(
            err.message
                .contains("origin/main already pins v0.50.0, which is >= target v0.49.0")
        );
        assert!(err.message.contains("pass --allow-redundant"));
    }

    #[test]
    fn run_checked_reports_nonzero_status() {
        let temp = TempDir::new().expect("tempdir");

        let err = run_checked(temp.path(), "false", &[]).expect_err("false exits non-zero");

        assert_eq!(err.code, 1);
        assert!(err.message.contains("false  exited with"));
    }

    #[test]
    fn run_checked_reports_spawn_failure() {
        let temp = TempDir::new().expect("tempdir");

        let err = run_checked(temp.path(), "definitely-not-a-shipyard-test-command", &[])
            .expect_err("spawn should fail");

        assert_eq!(err.code, 1);
        assert!(
            err.message
                .contains("definitely-not-a-shipyard-test-command failed")
        );
    }

    #[test]
    fn render_pin_noop_json_uses_stable_shape() {
        let mut out = Vec::new();

        render_pin_noop(&mut out, true, Some("v0.50.0"), "v0.50.0").expect("render should succeed");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "pin");
        assert_eq!(payload["event"], "bump");
        assert_eq!(payload["result"], "noop");
        assert_eq!(payload["current"], "v0.50.0");
        assert_eq!(payload["target"], "v0.50.0");
    }

    #[test]
    fn render_pin_bump_human_variants() {
        let temp = TempDir::new().expect("tempdir");
        let pin = ConsumerPin {
            pin_file: temp.path().join("tools").join("shipyard.toml"),
            repo_root: temp.path().to_path_buf(),
        };
        let mut no_pr = Vec::new();
        render_pin_bump(
            &mut no_pr,
            false,
            "edited",
            None,
            &pin,
            Some("v0.49.0"),
            "v0.50.0",
        )
        .expect("render should succeed");
        assert!(
            String::from_utf8(no_pr)
                .expect("utf8")
                .contains("--no-pr: edit left in the working tree")
        );

        let mut pr = Vec::new();
        render_pin_bump(
            &mut pr,
            false,
            "pr-opened",
            Some("https://github.com/danielraffel/pulp/pull/100"),
            &pin,
            Some("v0.49.0"),
            "v0.50.0",
        )
        .expect("render should succeed");
        assert_eq!(
            String::from_utf8(pr).expect("utf8"),
            "PR opened: https://github.com/danielraffel/pulp/pull/100\n"
        );
    }

    #[test]
    fn render_pin_bump_json_includes_pr_url_when_opened() {
        let temp = TempDir::new().expect("tempdir");
        let pin = ConsumerPin {
            pin_file: temp.path().join("tools").join("shipyard.toml"),
            repo_root: temp.path().to_path_buf(),
        };
        let mut out = Vec::new();

        render_pin_bump(
            &mut out,
            true,
            "pr-opened",
            Some("https://github.com/danielraffel/pulp/pull/100"),
            &pin,
            Some("v0.49.0"),
            "v0.50.0",
        )
        .expect("render succeeds");

        let payload: Value = serde_json::from_slice(&out).expect("json payload");
        assert_eq!(payload["command"], "pin");
        assert_eq!(payload["result"], "pr-opened");
        assert_eq!(payload["from"], "v0.49.0");
        assert_eq!(payload["to"], "v0.50.0");
        assert_eq!(
            payload["pr_url"],
            "https://github.com/danielraffel/pulp/pull/100"
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_AUTHOR_NAME", "Shipyard Test")
            .env("GIT_AUTHOR_EMAIL", "shipyard@example.invalid")
            .env("GIT_COMMITTER_NAME", "Shipyard Test")
            .env("GIT_COMMITTER_EMAIL", "shipyard@example.invalid")
            .status()
            .expect("git should run");
        assert!(
            status.success(),
            "git failed in {}: {args:?}",
            cwd.display()
        );
    }
}
