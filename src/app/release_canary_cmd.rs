//! `shipyard release-canary run`: rehearse the next release on a clean checkout
//! of the base branch and keep one tracking issue in step with the result.
//!
//! Read-only toward the repository: it clones, runs the configured command in
//! the clone, and throws the clone away. The only writes are to the one issue
//! the canary owns and to its own stamp file under the state directory.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant, SystemTime};

use chrono::Utc;
use fs2::FileExt;
use serde_json::Value;

use super::{CliFailure, branch_cmd::detect_repo_from_remote, cli::ReleaseCanaryCommand};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::marked_issue;
use crate::output::write_json_envelope;
use crate::process::{BoundedOutputError, run_output_until};
use crate::release_canary::{
    CanaryConfig, IssueAction, MARKER_PREFIX, Outcome, daily_due, decide, load_config,
    next_version, output_tail, render_command,
};

const CLONE_TIMEOUT: Duration = Duration::from_mins(10);
const GH_TIMEOUT: Duration = Duration::from_mins(1);

type Gh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

pub(super) fn release_canary_command<W: Write>(
    command: ReleaseCanaryCommand,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let ReleaseCanaryCommand::Run {
        repo,
        base,
        daily,
        apply,
    } = command;
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, error.to_string()))?;
    let Some(canary) = load_config(&config).map_err(|error| CliFailure::new(2, error))? else {
        return emit(
            json,
            stdout,
            "skipped",
            "",
            "",
            "no [release.canary] section",
            0,
        );
    };
    let repo = repo
        .or_else(|| detect_repo_from_remote(cwd, None))
        .ok_or_else(|| CliFailure::new(2, "Can't detect owner/repo; pass --repo."))?;
    let origin = git_origin(cwd).ok_or_else(|| CliFailure::new(2, "no git origin remote"))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config).with_repo_override(&repo);
    let gh = |argv: &[String]| {
        actions
            .run_gh_with_timeout_env(argv, GH_TIMEOUT, &[("GH_REPO", repo.as_str())])
            .map_err(|error| error.to_string())
    };
    let request = Request {
        repo: &repo,
        base: &base,
        origin: &origin,
        canary: &canary,
        work_root: &state_dir.join("release-canary"),
        daily,
        apply,
    };
    match run_canary(&request, &gh) {
        Ok(report) => {
            let code = u8::from(report.status == "fail");
            emit(
                json,
                stdout,
                report.status,
                &report.tag,
                &report.issue,
                &report.detail,
                code,
            )
        }
        Err(message) => Err(CliFailure::new(2, message)),
    }
}

fn emit<W: Write>(
    json: bool,
    stdout: &mut W,
    status: &str,
    tag: &str,
    issue: &str,
    detail: &str,
    code: u8,
) -> Result<ExitCode, CliFailure> {
    let io = |error: &dyn std::fmt::Display| CliFailure::new(2, error.to_string());
    if json {
        let mut data = BTreeMap::new();
        data.insert("status".to_owned(), Value::String(status.to_owned()));
        data.insert("tag".to_owned(), Value::String(tag.to_owned()));
        data.insert("issue".to_owned(), Value::String(issue.to_owned()));
        data.insert("detail".to_owned(), Value::String(detail.to_owned()));
        write_json_envelope(stdout, "release-canary:run", data).map_err(|error| io(&error))?;
    } else {
        writeln!(
            stdout,
            "release-canary status={status} tag={tag} issue={issue} {detail}"
        )
        .map_err(|error| io(&error))?;
    }
    Ok(ExitCode::from(code))
}

fn git_origin(cwd: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|url| !url.is_empty())
}

pub(crate) struct Request<'a> {
    pub repo: &'a str,
    pub base: &'a str,
    pub origin: &'a str,
    pub canary: &'a CanaryConfig,
    pub work_root: &'a Path,
    pub daily: bool,
    pub apply: bool,
}

#[derive(Debug)]
pub(crate) struct Report {
    /// `pass`, `fail` or `skipped`.
    pub status: &'static str,
    pub tag: String,
    pub issue: String,
    pub detail: String,
}

fn skipped(detail: &str) -> Report {
    Report {
        status: "skipped",
        tag: String::new(),
        issue: String::new(),
        detail: detail.to_owned(),
    }
}

/// Run one canary. `Err` is an infrastructure problem (clone, tags, GitHub),
/// never a canary failure: it opens no issue.
pub(crate) fn run_canary(request: &Request<'_>, gh: Gh<'_>) -> Result<Report, String> {
    fs::create_dir_all(request.work_root).map_err(|error| error.to_string())?;
    let slug = request.repo.replace('/', "_");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(request.work_root.join(format!("{slug}.lock")))
        .map_err(|error| error.to_string())?;
    if lock.try_lock_exclusive().is_err() {
        return Ok(skipped("another canary run holds the lock"));
    }
    let stamp_path = request.work_root.join(format!("{slug}.stamp"));
    let today = Utc::now().format("%Y-%m-%d").to_string();
    if request.daily {
        let content = fs::read_to_string(&stamp_path).ok();
        let age = fs::metadata(&stamp_path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map_or(0, |age| age.as_secs());
        if !daily_due(content.as_deref().map(|text| (text, age)), &today) {
            return Ok(skipped("already ran today"));
        }
    }
    let result = run_once(request, gh);
    if request.daily {
        let status = match &result {
            Ok(report) => report.status,
            Err(_) => "error",
        };
        let _ = fs::write(&stamp_path, format!("{today} {status}\n"));
    }
    let _ = FileExt::unlock(&lock);
    result
}

fn run_once(request: &Request<'_>, gh: Gh<'_>) -> Result<Report, String> {
    let scratch = tempfile::Builder::new()
        .prefix("clone-")
        .tempdir_in(request.work_root)
        .map_err(|error| error.to_string())?;
    let checkout = scratch.path().join("repo");
    clone_base(request, &checkout)?;
    let tags = git_lines(&checkout, &["tag", "--list"])?;
    let prefix = &request.canary.tag_prefix;
    let version = next_version(&tags, prefix, request.canary.bump)
        .ok_or_else(|| format!("no {prefix}X.Y.Z tag in {}", request.repo))?;
    let tag = format!("{prefix}{version}");
    let command = render_command(&request.canary.command, &version, prefix);
    let outcome = run_command(&command, &checkout, &version, request.canary.timeout_secs)?;

    let raw = gh(&marked_issue::list_open_args(request.repo))?;
    let existing: Vec<_> = marked_issue::parse(&raw, MARKER_PREFIX)
        .into_iter()
        .filter(|issue| issue.key == request.repo)
        .collect();
    let action = decide(&existing, request.repo, &tag, &command, &outcome);
    let issue = apply_action(request, gh, &action)?;
    let (status, detail) = match &outcome {
        Outcome::Pass => ("pass", "canary passed".to_owned()),
        Outcome::Fail { reason, .. } => ("fail", reason.clone()),
    };
    Ok(Report {
        status,
        tag,
        issue,
        detail,
    })
}

fn clone_base(request: &Request<'_>, checkout: &Path) -> Result<(), String> {
    let mut clone = Command::new("git");
    clone
        .args(["clone", "--quiet", "--filter=blob:none", "--branch"])
        .arg(request.base)
        .arg(request.origin)
        .arg(checkout)
        .env("GIT_TERMINAL_PROMPT", "0");
    let output = run_output_until(&mut clone, Instant::now() + CLONE_TIMEOUT, "git clone")
        .map_err(|error| error.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn git_lines(checkout: &Path, args: &[&str]) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(args)
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!("git {} failed", args.join(" ")));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToOwned::to_owned)
        .collect())
}

fn run_command(
    command: &str,
    checkout: &Path,
    version: &str,
    timeout_secs: u64,
) -> Result<Outcome, String> {
    let mut shell = Command::new("sh");
    shell
        .arg("-c")
        .arg(command)
        .current_dir(checkout)
        .env("SHIPYARD_RELEASE_CANARY", "1")
        .env("SHIPYARD_CANARY_NEXT_VERSION", version)
        .env("GIT_TERMINAL_PROMPT", "0");
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    match run_output_until(&mut shell, deadline, "release canary command") {
        Ok(output) if output.status.success() => Ok(Outcome::Pass),
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            Ok(Outcome::Fail {
                reason: output
                    .status
                    .code()
                    .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
                tail: output_tail(&text),
            })
        }
        Err(BoundedOutputError::TimedOut { .. }) => Ok(Outcome::Fail {
            reason: format!("timed out after {timeout_secs}s"),
            tail: String::new(),
        }),
        Err(error) => Err(error.to_string()),
    }
}

fn apply_action(request: &Request<'_>, gh: Gh<'_>, action: &IssueAction) -> Result<String, String> {
    let repo = request.repo;
    let args = |method: &str, path: String, fields: &[(&str, &str)]| {
        let mut argv = vec![
            "api".to_owned(),
            "--method".to_owned(),
            method.to_owned(),
            path,
        ];
        for (key, value) in fields {
            argv.push("-f".to_owned());
            argv.push(format!("{key}={value}"));
        }
        argv
    };
    let described = match action {
        IssueAction::Nothing => return Ok(String::new()),
        IssueAction::Open { .. } => "would open".to_owned(),
        IssueAction::Update { number, .. } => format!("would update #{number}"),
        IssueAction::Close { number, .. } => format!("would close #{number}"),
    };
    if !request.apply {
        return Ok(described);
    }
    match action {
        IssueAction::Nothing => Ok(String::new()),
        IssueAction::Open { title, body } => {
            let raw = gh(&args(
                "POST",
                format!("repos/{repo}/issues"),
                &[("title", title), ("body", body)],
            ))?;
            let number = serde_json::from_str::<Value>(&raw)
                .ok()
                .and_then(|value| value.get("number").and_then(Value::as_u64));
            Ok(number.map_or_else(|| "opened".to_owned(), |n| format!("opened #{n}")))
        }
        IssueAction::Update {
            number,
            title,
            body,
        } => {
            gh(&args(
                "PATCH",
                format!("repos/{repo}/issues/{number}"),
                &[("title", title), ("body", body)],
            ))?;
            Ok(format!("updated #{number}"))
        }
        IssueAction::Close { number, comment } => {
            gh(&args(
                "POST",
                format!("repos/{repo}/issues/{number}/comments"),
                &[("body", comment)],
            ))?;
            gh(&args(
                "PATCH",
                format!("repos/{repo}/issues/{number}"),
                &[("state", "closed"), ("state_reason", "completed")],
            ))?;
            Ok(format!("closed #{number}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::path::PathBuf;

    use super::*;
    use crate::release_canary::Bump;

    fn git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    }

    /// A local "origin" on main with tag v0.2.0 and a file `ok`.
    fn origin(dir: &Path) -> PathBuf {
        let path = dir.join("origin");
        fs::create_dir_all(&path).unwrap();
        git(&path, &["init", "-q", "-b", "main"]);
        fs::write(path.join("ok"), "x").unwrap();
        git(&path, &["add", "."]);
        git(&path, &["commit", "-q", "-m", "init"]);
        git(&path, &["tag", "v0.2.0"]);
        path
    }

    fn canary(command: &str, timeout_secs: u64) -> CanaryConfig {
        CanaryConfig {
            command: command.to_owned(),
            timeout_secs,
            bump: Bump::Patch,
            tag_prefix: "v".to_owned(),
        }
    }

    struct Fake {
        issues: String,
        calls: RefCell<Vec<Vec<String>>>,
    }

    impl Fake {
        fn new(issues: &str) -> Self {
            Self {
                issues: issues.to_owned(),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn gh(&self, argv: &[String]) -> String {
            self.calls.borrow_mut().push(argv.to_vec());
            if argv.iter().any(|a| a == "--paginate") {
                self.issues.clone()
            } else {
                "{\"number\": 7}".to_owned()
            }
        }
        fn mutations(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .filter(|argv| argv.iter().any(|a| a == "--method"))
                .map(|argv| argv[2..4].join(" "))
                .collect()
        }
    }

    fn run(
        dir: &Path,
        command: &str,
        timeout: u64,
        fake: &Fake,
        daily: bool,
    ) -> Result<Report, String> {
        let origin = origin_once(dir);
        let cfg = canary(command, timeout);
        let request = Request {
            repo: "o/r",
            base: "main",
            origin: origin.to_str().unwrap(),
            canary: &cfg,
            work_root: &dir.join("state"),
            daily,
            apply: true,
        };
        run_canary(&request, &|argv| Ok(fake.gh(argv)))
    }

    fn origin_once(dir: &Path) -> PathBuf {
        let path = dir.join("origin");
        if path.exists() { path } else { origin(dir) }
    }

    fn marked_issue_line() -> String {
        let body = format!("{MARKER_PREFIX}o/r -->\nold");
        format!("{}\n", serde_json::json!({"number": 3, "body": body}))
    }

    #[test]
    fn failing_command_opens_one_issue_with_the_next_tag_and_tail() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let report = run(
            dir.path(),
            "echo rehearsing {next_tag}; echo parity waiver missing >&2; exit 3",
            30,
            &fake,
            false,
        )
        .unwrap();
        assert_eq!(report.status, "fail");
        assert_eq!(report.tag, "v0.2.1");
        assert_eq!(fake.mutations(), ["POST repos/o/r/issues"]);
        let calls = fake.calls.borrow();
        let body = calls
            .iter()
            .flatten()
            .find(|a| a.starts_with("body="))
            .unwrap();
        assert!(body.contains("parity waiver missing"));
        assert!(body.contains("exit 3"));
        assert!(
            calls
                .iter()
                .flatten()
                .any(|a| a == "title=release canary: v0.2.1 would fail")
        );
    }

    #[test]
    fn failing_again_updates_and_passing_closes_the_same_issue() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new(&marked_issue_line());
        run(dir.path(), "exit 1", 30, &fake, false).unwrap();
        assert_eq!(fake.mutations(), ["PATCH repos/o/r/issues/3"]);

        let fake = Fake::new(&marked_issue_line());
        let report = run(dir.path(), "test -f ok", 30, &fake, false).unwrap();
        assert_eq!(report.status, "pass");
        assert_eq!(
            fake.mutations(),
            [
                "POST repos/o/r/issues/3/comments",
                "PATCH repos/o/r/issues/3"
            ]
        );
    }

    #[test]
    fn passing_with_no_issue_does_nothing_and_never_touches_the_origin() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let report = run(dir.path(), "test -f ok", 30, &fake, false).unwrap();
        assert_eq!(report.status, "pass");
        assert!(fake.mutations().is_empty());
        let head = Command::new("git")
            .arg("-C")
            .arg(dir.path().join("origin"))
            .args(["tag", "--list"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "v0.2.0");
    }

    #[test]
    fn a_timeout_is_a_failure() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let report = run(dir.path(), "sleep 30", 1, &fake, false).unwrap();
        assert_eq!(report.status, "fail");
        assert!(report.detail.contains("timed out"));
    }

    #[test]
    fn unreadable_issue_list_is_an_error_not_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let origin = origin(dir.path());
        let cfg = canary("true", 30);
        let request = Request {
            repo: "o/r",
            base: "main",
            origin: origin.to_str().unwrap(),
            canary: &cfg,
            work_root: &dir.path().join("state"),
            daily: false,
            apply: true,
        };
        let result = run_canary(&request, &|_| Err("HTTP 502".to_owned()));
        assert!(result.is_err());
    }

    #[test]
    fn daily_runs_once_per_day() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        assert_eq!(
            run(dir.path(), "true", 30, &fake, true).unwrap().status,
            "pass"
        );
        let second = run(dir.path(), "exit 1", 30, &fake, true).unwrap();
        assert_eq!(second.status, "skipped");
    }

    #[test]
    fn dry_run_mutates_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let origin = origin(dir.path());
        let cfg = canary("exit 1", 30);
        let request = Request {
            repo: "o/r",
            base: "main",
            origin: origin.to_str().unwrap(),
            canary: &cfg,
            work_root: &dir.path().join("state"),
            daily: false,
            apply: false,
        };
        let fake = Fake::new("");
        let report = run_canary(&request, &|argv| Ok(fake.gh(argv))).unwrap();
        assert_eq!(report.status, "fail");
        assert!(fake.mutations().is_empty());
    }
}
