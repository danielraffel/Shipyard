//! `shipyard claim`: the push claim fence (`check-push`, called by a git
//! pre-push hook) and its explicit release-and-take path (`take`). The
//! decision lives in [`crate::claim_fence`]; this module only does the I/O.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::CliFailure;
use super::cli::ClaimCommand;
use crate::claim_fence::{
    DONE_ENV, Fence, FenceEnv, FenceIo, already_checked, check_push, pushed_branches, take,
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
                Ok(repo) => Ok(report(&run_fence(&repo, &branches, None))),
                Err(note) => {
                    eprintln!("{note}");
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        ClaimCommand::Take {
            from,
            reason,
            pr,
            repo,
        } => {
            let repo = match repo {
                Some(repo) => repo,
                None => super::ship_cmd::git_repo_slug(cwd).ok_or_else(|| {
                    CliFailure::new(
                        2,
                        "origin is not a GitHub repository; pass --repo OWNER/REPO",
                    )
                })?,
            };
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
    match run_fence(&repo, &[branch.to_owned()], gh_override) {
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

fn run_fence(repo: &str, branches: &[String], gh_override: Option<&Path>) -> Fence {
    if branches.is_empty() {
        return Fence::Allow;
    }
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
    let outcome = check_push(&io, &fence_env(), repo, branches);
    for note in &outcome.notes {
        eprintln!("{note}");
    }
    outcome.fence
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
