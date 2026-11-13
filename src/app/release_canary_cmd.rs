//! `shipyard release-canary run`: rehearse the next release on a clean checkout
//! of the base branch and keep one tracking issue in step with the result.
//!
//! Read-only toward the repository: it clones, runs the configured command in
//! the clone, and throws the clone away. The only writes are to the one issue
//! the canary owns and to its own stamp file under the state directory.
//!
//! The child is not a sandbox, but it is given nothing to publish with: the
//! clone has no remote, git's ssh and credential helpers are disabled, and
//! tokens and keys are stripped from its environment.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Utc;
use fs2::FileExt;
use serde_json::Value;

use super::{CliFailure, branch_cmd::detect_repo_from_remote, cli::ReleaseCanaryCommand};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;
use crate::marked_issue;
use crate::output::write_json_envelope;
use crate::process::{ProcessTree, run_output_until};
use crate::release_canary::{
    CanaryConfig, IssueAction, MARKER_PREFIX, Outcome, daily_due, decide, is_secret_env,
    load_config, next_version, output_tail, render_command,
};

const CLONE_TIMEOUT: Duration = Duration::from_mins(10);
const GH_TIMEOUT: Duration = Duration::from_mins(1);
/// The command's output is cut off, and the run failed, past this size.
const OUTPUT_CAP_BYTES: u64 = 64 * 1024 * 1024;
/// Bytes of the output file read back for the issue tail.
const READ_BACK_BYTES: u64 = 64 * 1024;

type Gh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;
type LoadCanary<'a> = &'a dyn Fn(&Path) -> Result<Option<CanaryConfig>, String>;

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
    let detected = detect_repo_from_remote(cwd, None);
    let repo = repo
        .or_else(|| detected.clone())
        .ok_or_else(|| CliFailure::new(2, "Can't detect owner/repo; pass --repo."))?;
    // Two owners can share a repository name, and the caller picks a config
    // clone by name alone: refuse a clone that belongs to another repository.
    if let Some(found) = &detected
        && !found.eq_ignore_ascii_case(&repo)
    {
        return Err(CliFailure::new(
            2,
            format!("{} is a clone of {found}, not {repo}", cwd.display()),
        ));
    }
    let origin = git_origin(cwd).ok_or_else(|| CliFailure::new(2, "no git origin remote"))?;
    let actions = GitHubActions::from_loaded_config(cwd, &config).with_repo_override(&repo);
    let gh = |argv: &[String]| {
        actions
            .run_gh_with_timeout_env(argv, GH_TIMEOUT, &[("GH_REPO", repo.as_str())])
            .map_err(|error| error.to_string())
    };
    // The canary section comes from the fresh clone of the base branch, not
    // from the local config clone, which can lag behind main.
    let load = |checkout: &Path| {
        let loaded = LoadedConfig::load_from_cwd(mode, checkout).map_err(|e| e.to_string())?;
        load_config(&loaded)
    };
    let request = Request {
        repo: &repo,
        base: &base,
        origin: &origin,
        load_canary: &load,
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
    /// Reads `[release.canary]` from the fresh checkout.
    pub load_canary: LoadCanary<'a>,
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

fn epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Run one canary. `Err` is an infrastructure problem (clone, GitHub), never a
/// canary failure: it opens no issue.
pub(crate) fn run_canary(request: &Request<'_>, gh: Gh<'_>) -> Result<Report, String> {
    fs::create_dir_all(request.work_root).map_err(|error| error.to_string())?;
    let slug = request.repo.replace('/', "_");
    // Released explicitly on every path, early returns included (see file_lock).
    let lock = crate::file_lock::LockedFile::new(
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(request.work_root.join(format!("{slug}.lock")))
            .map_err(|error| error.to_string())?,
    );
    if lock.try_lock_exclusive().is_err() {
        return Ok(skipped("another canary run holds the lock"));
    }
    let stamp_path = request.work_root.join(format!("{slug}.stamp"));
    let today = Utc::now().format("%Y-%m-%d").to_string();
    let stamping = request.daily && request.apply;
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
    // We hold the lock, so every `clone-*` directory is a leak from a run that
    // was killed before it could clean up.
    sweep_stale_clones(request.work_root);
    if stamping {
        // Written before the run: if the host kills this process, the next
        // tick sees a recent `running` stamp instead of starting over.
        let _ = fs::write(&stamp_path, format!("{today} running {}\n", epoch_secs()));
    }
    let result = run_once(request, gh);
    if stamping {
        let status = match &result {
            Ok(report) => report.status,
            Err(_) => "error",
        };
        let _ = fs::write(&stamp_path, format!("{today} {status}\n"));
    }
    result
}

fn sweep_stale_clones(work_root: &Path) {
    let Ok(entries) = fs::read_dir(work_root) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("clone-") {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn run_once(request: &Request<'_>, gh: Gh<'_>) -> Result<Report, String> {
    let scratch = tempfile::Builder::new()
        .prefix("clone-")
        .tempdir_in(request.work_root)
        .map_err(|error| error.to_string())?;
    let checkout = scratch.path().join("repo");
    clone_base(request, &checkout)?;
    let Some(canary) = (request.load_canary)(&checkout)? else {
        return Ok(skipped("no [release.canary] section on the base branch"));
    };
    let tags = git_lines(&checkout, &["tag", "--list"])?;
    let prefix = &canary.tag_prefix;
    let Some(version) = next_version(&tags, prefix, canary.bump) else {
        // Nothing to bump from: not a failure and not worth retrying today.
        return Ok(skipped(&format!(
            "no {prefix}X.Y.Z tag in {}",
            request.repo
        )));
    };
    let tag = format!("{prefix}{version}");
    let command = render_command(&canary.command, &version, prefix);
    let outcome = run_command(
        &command,
        &checkout,
        &version,
        canary.timeout_secs,
        scratch.path(),
    )?;

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
    if !output.status.success() {
        return Err(format!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // Cut the clone off from the real repository before anything runs in it.
    // The history and tags are already local; a push has nowhere to go.
    let removed = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["remote", "remove", "origin"])
        .output()
        .map_err(|error| error.to_string())?;
    if removed.status.success() {
        Ok(())
    } else {
        Err("could not detach the clone from origin".to_owned())
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

/// The last `READ_BACK_BYTES` of `file`, lossily decoded.
fn read_back(file: &mut File) -> String {
    let length = file.metadata().map_or(0, |meta| meta.len());
    let start = length.saturating_sub(READ_BACK_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_ok() {
        let _ = file.take(READ_BACK_BYTES).read_to_end(&mut bytes);
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn run_command(
    command: &str,
    checkout: &Path,
    version: &str,
    timeout_secs: u64,
    scratch: &Path,
) -> Result<Outcome, String> {
    let output_path = scratch.join("canary-output.log");
    let mut sink = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&output_path)
        .map_err(|error| error.to_string())?;
    let mut shell = Command::new("sh");
    shell
        .arg("-c")
        .arg(command)
        .current_dir(checkout)
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            sink.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(
            sink.try_clone().map_err(|error| error.to_string())?,
        ));
    for (key, _) in std::env::vars_os() {
        if key.to_str().is_none_or(is_secret_env) {
            shell.env_remove(key);
        }
    }
    shell
        // git cannot reach any remote over ssh, and has no credential helper.
        .env("GIT_SSH_COMMAND", "false")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        .env("SHIPYARD_RELEASE_CANARY", "1")
        .env("SHIPYARD_CANARY_NEXT_VERSION", version);
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut tree = ProcessTree::spawn(&mut shell).map_err(|error| error.to_string())?;
    let (reason, status) = loop {
        match tree.try_wait() {
            Ok(Some(status)) => break (None, Some(status)),
            Ok(None) => {}
            Err(error) => {
                tree.terminate_until(Instant::now() + Duration::from_secs(5));
                return Err(error.to_string());
            }
        }
        if Instant::now() >= deadline {
            break (Some(format!("timed out after {timeout_secs}s")), None);
        }
        if sink.metadata().map_or(0, |meta| meta.len()) > OUTPUT_CAP_BYTES {
            break (
                Some(format!("output exceeded {OUTPUT_CAP_BYTES} bytes; stopped")),
                None,
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    // Reap the whole tree whatever happened, so no descendant outlives the run.
    tree.terminate_until(Instant::now() + Duration::from_secs(5));
    let tail = output_tail(&read_back(&mut sink));
    let _ = sink.flush();
    Ok(match (reason, status) {
        (Some(reason), _) => Outcome::Fail { reason, tail },
        (None, Some(status)) if status.success() => Outcome::Pass,
        (None, Some(status)) => Outcome::Fail {
            reason: status
                .code()
                .map_or_else(|| "killed by a signal".to_owned(), |c| format!("exit {c}")),
            tail,
        },
        (None, None) => Outcome::Fail {
            reason: "no exit status".to_owned(),
            tail,
        },
    })
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
            // Close first: a failure leaves the issue open and the next run
            // retries the whole close. The comment is best effort, so a failed
            // one is never posted twice.
            gh(&args(
                "PATCH",
                format!("repos/{repo}/issues/{number}"),
                &[("state", "closed"), ("state_reason", "completed")],
            ))?;
            let _ = gh(&args(
                "POST",
                format!("repos/{repo}/issues/{number}/comments"),
                &[("body", comment)],
            ));
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
            load_canary: &|_| Ok(Some(cfg.clone())),
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
            "echo rehearsing {next_tag}; echo parity-$((1+1))-missing >&2; exit 3",
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
        assert!(body.contains("parity-2-missing"));
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
                "PATCH repos/o/r/issues/3",
                "POST repos/o/r/issues/3/comments"
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
            load_canary: &|_| Ok(Some(cfg.clone())),
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
            load_canary: &|_| Ok(Some(cfg.clone())),
            work_root: &dir.path().join("state"),
            daily: false,
            apply: false,
        };
        let fake = Fake::new("");
        let report = run_canary(&request, &|argv| Ok(fake.gh(argv))).unwrap();
        assert_eq!(report.status, "fail");
        assert!(fake.mutations().is_empty());
    }

    fn request<'a>(
        origin: &'a Path,
        load: LoadCanary<'a>,
        state: &'a Path,
        daily: bool,
    ) -> Request<'a> {
        Request {
            repo: "o/r",
            base: "main",
            origin: origin.to_str().unwrap(),
            load_canary: load,
            work_root: state,
            daily,
            apply: true,
        }
    }

    #[test]
    fn the_command_cannot_push_a_tag_to_the_real_origin() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let report = run(
            dir.path(),
            "git push origin HEAD:refs/tags/leaked && exit 0; exit 7",
            30,
            &fake,
            false,
        )
        .unwrap();
        assert_eq!(report.status, "fail");
        assert!(report.detail.contains("exit 7"));
        let tags = Command::new("git")
            .arg("-C")
            .arg(dir.path().join("origin"))
            .args(["tag", "--list"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&tags.stdout).trim(), "v0.2.0");
    }

    #[test]
    fn verbose_failures_and_timeouts_keep_the_captured_tail() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        let report = run(
            dir.path(),
            "yes noise | head -c 20000000; echo FINAL-$((2*3)); exit 1",
            120,
            &fake,
            false,
        )
        .unwrap();
        assert_eq!(report.status, "fail");
        let calls = fake.calls.borrow();
        let body = calls
            .iter()
            .flatten()
            .find(|a| a.starts_with("body="))
            .unwrap();
        assert!(body.contains("FINAL-6"));
        drop(calls);

        let fake = Fake::new("");
        let report = run(
            dir.path(),
            "echo partial-$((1+1)); sleep 30",
            1,
            &fake,
            false,
        )
        .unwrap();
        assert!(report.detail.contains("timed out"));
        let calls = fake.calls.borrow();
        let body = calls
            .iter()
            .flatten()
            .find(|a| a.starts_with("body="))
            .unwrap();
        assert!(body.contains("partial-2"));
    }

    #[test]
    fn no_tag_is_a_daily_skip_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let bare = dir.path().join("origin");
        fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "-q", "-b", "main"]);
        fs::write(bare.join("f"), "x").unwrap();
        git(&bare, &["add", "."]);
        git(&bare, &["commit", "-q", "-m", "init"]);
        let cfg = canary("true", 30);
        let state = dir.path().join("state");
        let load = |_: &Path| Ok(Some(cfg.clone()));
        let req = request(&bare, &load, &state, true);
        let report = run_canary(&req, &|_| Ok(String::new())).unwrap();
        assert_eq!(report.status, "skipped");
        let stamp = fs::read_to_string(state.join("o_r.stamp")).unwrap();
        assert!(stamp.trim().ends_with("skipped"), "{stamp}");
    }

    #[test]
    fn no_section_on_the_fresh_clone_is_a_skip() {
        let dir = tempfile::tempdir().unwrap();
        let origin = origin(dir.path());
        let state = dir.path().join("state");
        let load = |_: &Path| Ok(None);
        let req = request(&origin, &load, &state, false);
        let report = run_canary(&req, &|_| Err("must not be called".to_owned())).unwrap();
        assert_eq!(report.status, "skipped");
    }

    #[test]
    fn a_running_stamp_blocks_reruns_and_stale_clones_are_swept() {
        let dir = tempfile::tempdir().unwrap();
        let origin = origin(dir.path());
        let cfg = canary("true", 30);
        let state = dir.path().join("state");
        fs::create_dir_all(state.join("clone-leaked")).unwrap();
        let today = Utc::now().format("%Y-%m-%d").to_string();
        fs::write(state.join("o_r.stamp"), format!("{today} running 1\n")).unwrap();
        let load = |_: &Path| Ok(Some(cfg.clone()));
        let req = request(&origin, &load, &state, true);
        let report = run_canary(&req, &|_| Err("unused".to_owned())).unwrap();
        assert_eq!(report.status, "skipped");
        assert!(state.join("clone-leaked").exists(), "skip must not sweep");

        fs::remove_file(state.join("o_r.stamp")).unwrap();
        run_canary(&req, &|_| Ok(String::new())).unwrap();
        assert!(!state.join("clone-leaked").exists());
        let stamp = fs::read_to_string(state.join("o_r.stamp")).unwrap();
        assert!(stamp.trim().ends_with("pass"), "{stamp}");
    }

    #[test]
    fn a_failed_close_is_retried_whole_and_never_comments_first() {
        let dir = tempfile::tempdir().unwrap();
        let origin = origin(dir.path());
        let cfg = canary("true", 30);
        let state = dir.path().join("state");
        let load = |_: &Path| Ok(Some(cfg.clone()));
        let req = request(&origin, &load, &state, false);
        let calls = RefCell::new(Vec::<String>::new());
        let issues = marked_issue_line();
        let result = run_canary(&req, &|argv| {
            if argv.iter().any(|a| a == "--paginate") {
                return Ok(issues.clone());
            }
            calls.borrow_mut().push(argv[2..4].join(" "));
            Err("HTTP 502".to_owned())
        });
        assert!(result.is_err());
        assert_eq!(*calls.borrow(), ["PATCH repos/o/r/issues/3"]);
    }

    fn alive(pid: &str) -> bool {
        Command::new("kill")
            .args(["-0", pid])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    #[test]
    fn a_timed_out_command_leaves_no_surviving_descendant() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("child.pid");
        let fake = Fake::new("");
        let command = format!("sleep 300 & echo $! > {}; sleep 30", pid_file.display());
        let report = run(dir.path(), &command, 1, &fake, false).unwrap();
        assert!(report.detail.contains("timed out"));
        let pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();
        let mut gone = false;
        for _ in 0..50 {
            if !alive(&pid) {
                gone = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(gone, "background child {pid} survived the timeout");
    }

    #[test]
    fn a_run_killed_midway_leaves_a_stamp_that_blocks_the_next_daily_run() {
        let dir = tempfile::tempdir().unwrap();
        let fake = Fake::new("");
        // While the command runs, the stamp already says `running`.
        let report = run(dir.path(), "cat ../../o_r.stamp; exit 1", 30, &fake, true).unwrap();
        assert_eq!(report.status, "fail");
        let calls = fake.calls.borrow();
        let body = calls
            .iter()
            .flatten()
            .find(|a| a.starts_with("body="))
            .unwrap();
        let running = body
            .lines()
            .find(|line| line.contains(" running "))
            .unwrap_or_else(|| panic!("no running stamp in: {body}"))
            .to_owned();
        drop(calls);

        // Simulate the host killing the process: the stamp is what it left.
        let state = dir.path().join("state");
        fs::write(state.join("o_r.stamp"), format!("{running}\n")).unwrap();
        let cfg = canary("true", 30);
        let load = |_: &Path| -> Result<Option<CanaryConfig>, String> {
            panic!("a clone and config read started despite the in-progress stamp")
        };
        let _ = &cfg;
        let origin = dir.path().join("origin");
        let req = request(&origin, &load, &state, true);
        let report = run_canary(&req, &|_| Err("must not be called".to_owned())).unwrap();
        assert_eq!(report.status, "skipped");
        assert_eq!(report.detail, "already ran today");
    }
}
