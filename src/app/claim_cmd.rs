//! `shipyard claim`: the push claim fence (`check-push`, called by a git
//! pre-push hook), a session's own claim (`post`, `release`), and the explicit
//! take of another's (`take`). The decision lives in [`crate::claim_fence`];
//! this module only does the I/O.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::CliFailure;
use super::cli::ClaimCommand;
use crate::claim_fence::{
    ClaimRecord, DONE_ENV, Fence, FenceEnv, FenceIo, RECORD_KEY_PATTERN, already_checked,
    check_push, parse_until, post, pushed_branches, release, take,
};

/// `ghread`'s answer for a repository it does not serve (`danielraffel/*`).
const GHREAD_NOT_SERVED: i32 = 75;
/// A write (the take) may not hang longer than this.
const WRITE_TIMEOUT: Duration = Duration::from_secs(20);

pub(super) fn claim_command<W: Write>(
    command: ClaimCommand,
    cwd: &Path,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    match command {
        ClaimCommand::CheckPush {
            remote: _,
            url,
            branches,
        } => {
            let env = FenceEnv::from_env(&|name| std::env::var(name).ok());
            let branches = if branches.is_empty() {
                let mut stdin = String::new();
                std::io::stdin()
                    .read_to_string(&mut stdin)
                    .map_err(|error| {
                        CliFailure::new(2, format!("cannot read the hook's stdin: {error}"))
                    })?;
                if already_checked(&stdin, std::env::var(DONE_ENV).ok().as_deref()) {
                    return Ok(ExitCode::SUCCESS);
                }
                pushed_branches(&stdin)
            } else {
                branches
            };
            if !env.enabled {
                return Ok(ExitCode::SUCCESS);
            }
            match github_target(&url) {
                Ok(repo) => Ok(report(&run_fence(cwd, &repo, &branches, None))),
                Err(note) => {
                    eprintln!("{note}");
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        ClaimCommand::Post {
            pr,
            repo,
            owner,
            until,
        } => post_command(cwd, stdout, pr, repo, &owner, &until),
        ClaimCommand::Release { pr, repo } => release_command(cwd, stdout, pr, repo),
        ClaimCommand::Take {
            from,
            reason,
            pr,
            repo,
        } => {
            let repo = repo_or_origin(cwd, repo)?;
            let read = |repo: &str, args: &[String], timeout: Duration| {
                fleet_gh(repo, args, None, timeout)
            };
            let started = Instant::now();
            let elapsed = || started.elapsed();
            let io = FenceIo {
                read: &read,
                sleep: &std::thread::sleep,
                elapsed: &elapsed,
                now: Utc::now(),
            };
            let pr = match pr {
                Some(pr) => pr,
                None => current_branch_pr(cwd, &io, &repo)?,
            };
            let write = |repo: &str, args: &[String]| {
                run_tool("ghapp", repo, args, true, WRITE_TIMEOUT).map_err(|error| error.text)
            };
            let env = FenceEnv::from_env(&|name| std::env::var(name).ok());
            let posted = take(&io, &write, &env, &repo, pr, &from, &reason)
                .map_err(|error| CliFailure::new(1, format!("claim take refused: {}", error.0)))?;
            writeln!(stdout, "{posted}").map_err(|error| CliFailure::new(1, error.to_string()))?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// `shipyard claim post`: post this session's claim, then record that exact
/// comment in this worktree's own config.
fn post_command<W: Write>(
    cwd: &Path,
    stdout: &mut W,
    pr: u64,
    repo: Option<String>,
    owner: &str,
    until: &str,
) -> Result<ExitCode, CliFailure> {
    let repo = repo_or_origin(cwd, repo)?;
    let now = Utc::now();
    let until = parse_until(until, now)
        .map_err(|error| CliFailure::new(2, format!("claim post refused: {error}")))?;
    let read = |repo: &str, args: &[String], timeout: Duration| fleet_gh(repo, args, None, timeout);
    let started = Instant::now();
    let elapsed = || started.elapsed();
    let io = FenceIo {
        read: &read,
        sleep: &std::thread::sleep,
        elapsed: &elapsed,
        now,
    };
    let write = |repo: &str, args: &[String]| {
        run_tool("ghapp", repo, args, true, WRITE_TIMEOUT).map_err(|error| error.text)
    };
    let (record, posted) = post(&io, &write, &repo, pr, owner, until)
        .map_err(|error| CliFailure::new(1, format!("claim post refused: {}", error.0)))?;
    write_record(cwd, &record).map_err(|error| {
        CliFailure::new(
            1,
            format!(
                "{posted}\nclaim post: the claim is posted but this worktree could not \
                 record it ({error}), so the fence will not count it as yours; post \
                 `repair-claim-released: {owner}` on {repo}#{pr} and try again"
            ),
        )
    })?;
    writeln!(
        stdout,
        "{posted}\nrecorded in this worktree: {} = {}",
        ClaimRecord::key(&repo, pr),
        record.value()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

/// `shipyard claim release`: release the claim this worktree recorded.
fn release_command<W: Write>(
    cwd: &Path,
    stdout: &mut W,
    pr: u64,
    repo: Option<String>,
) -> Result<ExitCode, CliFailure> {
    let repo = repo_or_origin(cwd, repo)?;
    let record = read_records(cwd)
        .into_iter()
        .find(|record| record.repo == repo.to_ascii_lowercase() && record.pr == pr)
        .ok_or_else(|| {
            CliFailure::new(
                1,
                format!("claim release refused: this worktree has no claim record for {repo}#{pr}"),
            )
        })?;
    let read = |repo: &str, args: &[String], timeout: Duration| fleet_gh(repo, args, None, timeout);
    let started = Instant::now();
    let elapsed = || started.elapsed();
    let io = FenceIo {
        read: &read,
        sleep: &std::thread::sleep,
        elapsed: &elapsed,
        now: Utc::now(),
    };
    let write = |repo: &str, args: &[String]| {
        run_tool("ghapp", repo, args, true, WRITE_TIMEOUT).map_err(|error| error.text)
    };
    let posted = release(&io, &write, &repo, &record)
        .map_err(|error| CliFailure::new(1, format!("claim release refused: {}", error.0)))?;
    remove_record(cwd, &repo, pr).map_err(|error| {
        CliFailure::new(
            1,
            format!("{posted}\nclaim release: the record could not be removed: {error}"),
        )
    })?;
    writeln!(stdout, "{posted}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(ExitCode::SUCCESS)
}

/// The fence before `shipyard pr`/ship pushes `branch`: a refusal is a
/// failure; anything else is reported on stderr and the push goes ahead.
/// `gh_override` is the command's own `--gh-command`, when given. Returns the
/// environment to hand the push, so the repository's pre-push hook skips the
/// head this already checked.
pub(super) fn fence_before_push(
    cwd: &Path,
    branch: &str,
    gh_override: Option<&Path>,
) -> Result<Vec<(OsString, OsString)>, CliFailure> {
    if !fence_env().enabled {
        return Ok(Vec::new());
    }
    let Some(repo) = super::ship_cmd::git_repo_slug(cwd) else {
        return Ok(Vec::new());
    };
    match run_fence(cwd, &repo, &[branch.to_owned()], gh_override) {
        Fence::Refuse(text) => Err(CliFailure::new(1, text)),
        other => {
            if let Some(message) = other.message() {
                eprintln!("{message}");
            }
            Ok(head_sha(cwd)
                .map(|sha| vec![(OsString::from(DONE_ENV), OsString::from(sha))])
                .unwrap_or_default())
        }
    }
}

#[cfg(test)]
thread_local! {
    /// A test's `SHIPYARD_CLAIM_FENCE=on`, for this thread only.
    pub(super) static TEST_FENCE_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// This process's fence settings (and, in a test, this thread's switch).
fn fence_env() -> FenceEnv {
    let env = FenceEnv::from_env(&|name| std::env::var(name).ok());
    #[cfg(test)]
    let env = FenceEnv {
        enabled: env.enabled || TEST_FENCE_ON.with(std::cell::Cell::get),
        ..env
    };
    env
}

fn head_sha(cwd: &Path) -> Option<String> {
    let output = crate::supervised::git_supervised()
        .args(["rev-parse", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && sha.len() == 40).then_some(sha)
}

fn run_fence(cwd: &Path, repo: &str, branches: &[String], gh_override: Option<&Path>) -> Fence {
    if branches.is_empty() {
        return Fence::Allow;
    }
    let env = FenceEnv {
        records: read_records(cwd),
        ..fence_env()
    };
    let read =
        |repo: &str, args: &[String], timeout: Duration| fleet_gh(repo, args, gh_override, timeout);
    let started = Instant::now();
    let elapsed = || started.elapsed();
    let io = FenceIo {
        read: &read,
        sleep: &std::thread::sleep,
        elapsed: &elapsed,
        now: Utc::now(),
    };
    let outcome = check_push(&io, &env, repo, branches);
    for note in &outcome.notes {
        eprintln!("{note}");
    }
    outcome.fence
}

fn repo_or_origin(cwd: &Path, repo: Option<String>) -> Result<String, CliFailure> {
    match repo {
        Some(repo) => Ok(repo),
        None => super::ship_cmd::git_repo_slug(cwd).ok_or_else(|| {
            CliFailure::new(
                2,
                "origin is not a GitHub repository; pass --repo OWNER/REPO",
            )
        }),
    }
}

/// `git` in `cwd`, finding its repository from `cwd` alone: a hook exports
/// `GIT_DIR`, and a `-c` from the caller must not reach the config read.
fn git_here(cwd: &Path) -> Command {
    let mut command =
        crate::config::clear_repository_selecting_env(crate::supervised::git_supervised());
    command.current_dir(cwd);
    command
}

/// The claim records in this worktree's own config (`config.worktree`).
/// A record anywhere else (the clone's shared `.git/config`, a global or a
/// `-c`) is not this worktree's and does not count: git reads
/// `config.worktree` only with `extensions.worktreeConfig`, and
/// `--show-scope` says which file each entry came from.
fn read_records(cwd: &Path) -> Vec<ClaimRecord> {
    let Ok(output) = git_here(cwd)
        .args(["config", "--show-scope", "--get-regexp", RECORD_KEY_PATTERN])
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("worktree\t"))
        .filter_map(ClaimRecord::parse)
        .collect()
}

fn git_ok(command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("git could not run: {error}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

/// Write `record` to this worktree's own config, turning on
/// `extensions.worktreeConfig` first if it is off: without it `--worktree`
/// writes the clone's shared config, which every worktree reads.
///
/// With the extension on, git stops reading `core.bare` and `core.worktree`
/// from the shared config as the main worktree's own, so a bare clone's linked
/// worktrees would stop working and a shared `core.worktree` would move every
/// worktree's top level. Like git's own `worktree add`, this first moves them
/// to the main worktree's `config.worktree`.
fn write_record(cwd: &Path, record: &ClaimRecord) -> Result<(), String> {
    let enabled =
        git_ok(git_here(cwd).args(["config", "--type=bool", "extensions.worktreeConfig"]))
            .is_ok_and(|value| value == "true");
    if !enabled {
        enable_worktree_config(cwd)?;
    }
    git_ok(git_here(cwd).args([
        "config",
        "--worktree",
        &ClaimRecord::key(&record.repo, record.pr),
        &record.value(),
    ]))
    .map(drop)
}

/// Turn on `extensions.worktreeConfig`, moving a shared `core.bare=true` and
/// `core.worktree` to the main worktree's `config.worktree` first.
fn enable_worktree_config(cwd: &Path) -> Result<(), String> {
    let common =
        git_ok(git_here(cwd).args(["rev-parse", "--path-format=absolute", "--git-common-dir"]))?;
    // Every `--file` edit runs from the common git dir: run from a linked
    // worktree, git would resolve a relative `core.worktree` against that
    // worktree once the extension is on, and die before the edit.
    let common_dir = Path::new(&common);
    let shared = Path::new(&common).join("config");
    let main_worktree = Path::new(&common).join("config.worktree");
    let shared_arg = shared.to_string_lossy().into_owned();
    let main_arg = main_worktree.to_string_lossy().into_owned();
    let read = |args: &[&str]| -> Option<String> {
        git_ok(
            git_here(common_dir)
                .args(["config", "--file", &shared_arg])
                .args(args),
        )
        .ok()
        .filter(|value| !value.is_empty())
    };
    let bare = read(&["--type=bool", "core.bare"]).is_some_and(|value| value == "true");
    let worktree = read(&["core.worktree"]);
    if bare {
        git_ok(git_here(common_dir).args(["config", "--file", &main_arg, "core.bare", "true"]))?;
    }
    if let Some(worktree) = &worktree {
        git_ok(git_here(common_dir).args([
            "config",
            "--file",
            &main_arg,
            "core.worktree",
            worktree,
        ]))?;
    }
    git_ok(git_here(common_dir).args([
        "config",
        "--file",
        &shared_arg,
        "extensions.worktreeConfig",
        "true",
    ]))?;
    if bare {
        git_ok(git_here(common_dir).args([
            "config",
            "--file",
            &shared_arg,
            "--unset",
            "core.bare",
        ]))?;
    }
    if worktree.is_some() {
        git_ok(git_here(common_dir).args([
            "config",
            "--file",
            &shared_arg,
            "--unset",
            "core.worktree",
        ]))?;
    }
    Ok(())
}

fn remove_record(cwd: &Path, repo: &str, pr: u64) -> Result<(), String> {
    git_ok(git_here(cwd).args([
        "config",
        "--worktree",
        "--unset",
        &ClaimRecord::key(repo, pr),
    ]))
    .map(drop)
}

/// The `OWNER/REPO` a remote URL names, or the note to print when it names
/// none (an SSH host alias, a typo, a local path): that push is not checked.
fn github_target(url: &str) -> Result<String, String> {
    super::wait_cmd::parse_github_repo_slug(url)
        .ok_or_else(|| format!("claim-fence: {url} is not a GitHub remote; not checked"))
}

fn report(fence: &Fence) -> ExitCode {
    if let Some(message) = fence.message() {
        eprintln!("{message}");
    }
    if matches!(fence, Fence::Refuse(_)) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn current_branch_pr(cwd: &Path, io: &FenceIo<'_>, repo: &str) -> Result<u64, CliFailure> {
    let output = crate::supervised::git_supervised()
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .map_err(|error| CliFailure::new(2, format!("git could not run: {error}")))?;
    let branch = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if !output.status.success() || branch.is_empty() || branch == "HEAD" {
        return Err(CliFailure::new(2, "no current branch; pass --pr"));
    }
    let prs = crate::claim_fence::open_prs(io, repo, &branch)
        .map_err(|error| CliFailure::new(1, format!("cannot find the pull request: {error}")))?;
    match prs.as_slice() {
        [pr] => Ok(*pr),
        [] => Err(CliFailure::new(
            2,
            format!("{branch} has no open pull request; pass --pr"),
        )),
        _ => Err(CliFailure::new(
            2,
            format!("{branch} has several open pull requests; pass --pr"),
        )),
    }
}

/// A read through the fleet's App clients: `ghapp` for `danielraffel/*`,
/// `ghread` (its own budget) elsewhere, falling back to `ghapp` only when
/// `ghread` is missing or does not serve the repository (never after a
/// timeout: that would spend the fence's deadline twice).
fn fleet_gh(
    repo: &str,
    args: &[String],
    gh_override: Option<&Path>,
    timeout: Duration,
) -> Result<String, String> {
    if let Some(program) = gh_override {
        return run_tool(&program.to_string_lossy(), repo, args, false, timeout)
            .map_err(|error| error.text);
    }
    fleet_gh_with(repo, args, timeout, "ghread", "ghapp")
}

fn fleet_gh_with(
    repo: &str,
    args: &[String],
    timeout: Duration,
    ghread: &str,
    ghapp: &str,
) -> Result<String, String> {
    let read = |program: &str| run_tool(program, repo, args, false, timeout);
    let owner = repo.split('/').next().unwrap_or_default();
    if owner.eq_ignore_ascii_case("danielraffel") {
        return read(ghapp).map_err(|error| error.text);
    }
    match read(ghread) {
        Ok(out) => Ok(out),
        Err(ToolError {
            kind: ToolFailure::NotRun | ToolFailure::Exit(GHREAD_NOT_SERVED),
            ..
        }) => read(ghapp).map_err(|error| error.text),
        Err(error) => Err(error.text),
    }
}

/// How a tool call failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ToolFailure {
    /// It could not be started.
    NotRun,
    /// It ran past its timeout and was killed.
    TimedOut,
    /// It exited with this code.
    Exit(i32),
    /// Anything else (a signal, a wait error).
    Other,
}

#[derive(Debug)]
struct ToolError {
    kind: ToolFailure,
    text: String,
}

/// Run one `gh`-shaped tool bound to `repo`, for at most `timeout`.
fn run_tool(
    program: &str,
    repo: &str,
    args: &[String],
    write: bool,
    timeout: Duration,
) -> Result<String, ToolError> {
    let fail = |kind, text: String| ToolError { kind, text };
    let mut command = Command::new(program);
    command
        .args(args)
        .env("GH_REPO", repo)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if write {
        crate::queue_arm_gate::strip_queue_authority(&mut command);
    }
    let name = Path::new(program).file_name().map_or_else(
        || program.to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    let mut child = command.spawn().map_err(|error| {
        fail(
            ToolFailure::NotRun,
            format!("{name} could not run: {error}"),
        )
    })?;
    let mut out = child.stdout.take().expect("piped stdout");
    let mut err = child.stderr.take().expect("piped stderr");
    let out_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = out.read_to_end(&mut buffer);
        buffer
    });
    let err_reader = std::thread::spawn(move || {
        let mut buffer = Vec::new();
        let _ = err.read_to_end(&mut buffer);
        buffer
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(
                    ToolFailure::TimedOut,
                    format!("{name} timed out after {}s", timeout.as_secs()),
                ));
            }
            Err(error) => {
                return Err(fail(
                    ToolFailure::Other,
                    format!("{name} could not be waited on: {error}"),
                ));
            }
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if status.success() {
        return Ok(String::from_utf8_lossy(&stdout).into_owned());
    }
    let detail = String::from_utf8_lossy(&stderr);
    let detail = detail
        .trim()
        .lines()
        .last()
        .unwrap_or("no detail")
        .to_owned();
    let kind = status.code().map_or(ToolFailure::Other, ToolFailure::Exit);
    Err(fail(kind, format!("{name} exited with {status}: {detail}")))
}

#[cfg(test)]
mod tests;
