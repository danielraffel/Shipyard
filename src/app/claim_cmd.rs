//! `shipyard claim`: the push claim fence (`check-push`, called by a git
//! pre-push hook) and its explicit release-and-take path (`take`). The
//! decision lives in [`crate::claim_fence`]; this module only does the I/O.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;

use super::CliFailure;
use super::cli::ClaimCommand;
use crate::claim_fence::{Fence, FenceEnv, FenceIo, check_push, pushed_branches, take};

/// One GitHub read may not hold a push longer than this.
const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// `ghread`'s answer for a repository it does not serve (`danielraffel/*`).
const GHREAD_NOT_SERVED: i32 = 75;

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
            let branches = if branches.is_empty() {
                let mut stdin = String::new();
                std::io::stdin()
                    .read_to_string(&mut stdin)
                    .map_err(|error| {
                        CliFailure::new(2, format!("cannot read the hook's stdin: {error}"))
                    })?;
                pushed_branches(&stdin)
            } else {
                branches
            };
            let Some(repo) = super::wait_cmd::parse_github_repo_slug(&url) else {
                return Ok(ExitCode::SUCCESS);
            };
            let fence = run_fence(&repo, &branches, None);
            Ok(report(&fence))
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
            let read = |repo: &str, args: &[String]| fleet_gh(repo, args, None);
            let io = FenceIo {
                read: &read,
                sleep: &std::thread::sleep,
                now: Utc::now(),
            };
            let pr = match pr {
                Some(pr) => pr,
                None => current_branch_pr(cwd, &io, &repo)?,
            };
            let write = |repo: &str, args: &[String]| run_tool("ghapp", repo, args, true);
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
/// `gh_override` is the command's own `--gh-command`, when given.
pub(super) fn fence_before_push(
    cwd: &Path,
    branch: &str,
    gh_override: Option<&Path>,
) -> Result<(), CliFailure> {
    let Some(repo) = super::ship_cmd::git_repo_slug(cwd) else {
        return Ok(());
    };
    match run_fence(&repo, &[branch.to_owned()], gh_override) {
        Fence::Refuse(text) => Err(CliFailure::new(1, text)),
        other => {
            if let Some(message) = other.message() {
                eprintln!("{message}");
            }
            Ok(())
        }
    }
}

fn run_fence(repo: &str, branches: &[String], gh_override: Option<&Path>) -> Fence {
    if branches.is_empty() {
        return Fence::Allow;
    }
    let read = |repo: &str, args: &[String]| fleet_gh(repo, args, gh_override);
    let io = FenceIo {
        read: &read,
        sleep: &std::thread::sleep,
        now: Utc::now(),
    };
    let env = FenceEnv::from_env(&|name| std::env::var(name).ok());
    check_push(&io, &env, repo, branches)
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
/// `ghread` (its own budget) elsewhere, falling back to `ghapp` when `ghread`
/// is missing or does not serve the repository.
fn fleet_gh(repo: &str, args: &[String], gh_override: Option<&Path>) -> Result<String, String> {
    if let Some(program) = gh_override {
        return run_tool(&program.to_string_lossy(), repo, args, false);
    }
    let owner = repo.split('/').next().unwrap_or_default();
    if owner.eq_ignore_ascii_case("danielraffel") {
        return run_tool("ghapp", repo, args, false);
    }
    match run_tool_coded("ghread", repo, args, false) {
        Ok(out) => Ok(out),
        // Missing, or not serving this repository: the App client reads it.
        Err((None | Some(GHREAD_NOT_SERVED), _)) => run_tool("ghapp", repo, args, false),
        Err((_, error)) => Err(error),
    }
}

/// Run one `gh`-shaped tool bound to `repo`, for at most [`READ_TIMEOUT`].
fn run_tool(program: &str, repo: &str, args: &[String], write: bool) -> Result<String, String> {
    run_tool_coded(program, repo, args, write).map_err(|(_, error)| error)
}

/// [`run_tool`], with the exit code of a tool that ran and exited nonzero
/// (`None` when it could not run, timed out, or died on a signal).
fn run_tool_coded(
    program: &str,
    repo: &str,
    args: &[String],
    write: bool,
) -> Result<String, (Option<i32>, String)> {
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
    let mut child = command
        .spawn()
        .map_err(|error| (None, format!("{name} could not run: {error}")))?;
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
    let deadline = Instant::now() + READ_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err((
                    None,
                    format!("{name} timed out after {}s", READ_TIMEOUT.as_secs()),
                ));
            }
            Err(error) => return Err((None, format!("{name} could not be waited on: {error}"))),
        }
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).into_owned())
    } else {
        let detail = String::from_utf8_lossy(&stderr);
        let detail = detail.trim();
        let detail = detail.lines().last().unwrap_or("no detail");
        Err((
            status.code(),
            format!("{name} exited with {status}: {detail}"),
        ))
    }
}
