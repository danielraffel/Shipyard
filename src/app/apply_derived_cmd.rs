//! `shipyard apply-derived --pr N`: commit CI's expected derived files to a
//! pull request whose drift check failed.
//!
//! Reads, in order:
//! 1. The pull request: its head, head branch and base. A closed or fork pull
//!    request is refused.
//! 2. The base branch's `.shipyard/config.toml` `[derived_files]` table
//!    ([`crate::apply_derived`]). The pull request's own copy is never read:
//!    it could widen the allowlist.
//! 3. The newest failed workflow run at that exact head that carries a
//!    configured expected-files artifact, then that artifact's zip.
//!
//! The checkout (`--path`, default the current directory) must be clean and
//! at the pull request's head. The command writes only allowlisted regular
//! files, refuses a path that crosses a symlink or a tracked symlink or
//! submodule, and checks that nothing else changed. It commits with the fixed
//! message `chore: apply CI's expected derived files from run <id>` plus the
//! configured trailers. Every git call runs with hooks and fsmonitor off, so
//! no repository code runs.
//!
//! The default is a dry run: it prints what would change and writes nothing.
//! `--push` writes, commits and pushes the head branch (never forced). The
//! pull request's head is read again before writing and before pushing; a
//! head that moved is refused.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use super::CliFailure;
use crate::apply_derived::{self, DerivedFilesConfig, safe_zip};
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::identity::RuntimeMode;

/// The output schema.
pub(super) const SCHEMA: &str = "shipyard.apply-derived/v1";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_mins(2);

/// Parsed arguments.
pub(super) struct ApplyDerivedArgs {
    pub(super) pr: u64,
    pub(super) repo: Option<String>,
    pub(super) path: Option<PathBuf>,
    pub(super) push: bool,
    pub(super) json: bool,
}

/// A pull request as the command reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PrHead {
    pub(super) state: String,
    pub(super) head_sha: String,
    pub(super) head_ref: String,
    pub(super) head_repo: String,
    pub(super) base_ref: String,
}

/// One workflow run at the head.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RunAtHead {
    pub(super) id: u64,
    pub(super) conclusion: Option<String>,
}

/// One artifact of a run.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ArtifactRef {
    pub(super) id: u64,
    pub(super) name: String,
    pub(super) size: u64,
    pub(super) expired: bool,
    /// The producing run's head, when GitHub reports it.
    pub(super) head_sha: Option<String>,
}

/// The GitHub reads and the one download the command needs; faked in tests.
pub(super) trait DerivedGitHub {
    fn pull_request(&self, repo: &str, number: u64) -> Result<PrHead, String>;
    /// The base branch's `.shipyard/config.toml`, or `None` when it has none.
    fn base_config(&self, repo: &str, base: &str) -> Result<Option<String>, String>;
    fn runs_at_head(&self, repo: &str, head: &str) -> Result<Vec<RunAtHead>, String>;
    fn run_artifacts(&self, repo: &str, run_id: u64) -> Result<Vec<ArtifactRef>, String>;
    fn download_artifact(
        &self,
        repo: &str,
        artifact_id: u64,
        max_bytes: usize,
    ) -> Result<Vec<u8>, String>;
}

/// How one file changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Change {
    New,
    Modified,
    Unchanged,
}

/// One file the artifact carries.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct FileReport {
    pub(super) path: String,
    pub(super) change: Change,
    pub(super) added: usize,
    pub(super) removed: usize,
}

/// What the command did.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    /// Nothing differs from the checkout.
    NothingToApply,
    /// Planned only (the default).
    DryRun,
    /// Written, committed and pushed.
    Pushed,
}

/// The command's report.
#[derive(Clone, Debug, Serialize)]
pub(super) struct Report {
    pub(super) schema: &'static str,
    pub(super) repo: String,
    pub(super) pr: u64,
    pub(super) head: String,
    pub(super) head_ref: String,
    pub(super) run_id: u64,
    pub(super) artifacts: Vec<String>,
    pub(super) outcome: Outcome,
    pub(super) files: Vec<FileReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) commit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) pushed_to: Option<String>,
}

pub(super) fn apply_derived_command<W: Write>(
    args: &ApplyDerivedArgs,
    mode: RuntimeMode,
    cwd: &Path,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let checkout = args.path.clone().unwrap_or_else(|| cwd.to_path_buf());
    let git = Git::new(&checkout)?;
    let repo = match args
        .repo
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
    {
        Some(repo) => repo.to_owned(),
        None => git
            .origin_repo()
            .ok_or_else(|| CliFailure::new(2, "No repo detected. Pass --repo OWNER/REPO."))?,
    };
    // Machine-global config only: the checkout's own `.shipyard/` belongs to
    // the pull request and must not choose the `gh` binary or token helper.
    let config = LoadedConfig::load_machine_global(mode)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let actions =
        GitHubActions::from_loaded_config(std::env::temp_dir(), &config).with_repo_override(&repo);
    let live = LiveGitHub { actions };
    let report = run(&live, &git, &repo, args.pr, args.push)
        .map_err(|error| CliFailure::new(1, format!("apply-derived refused: {error}")))?;
    if args.json {
        let text = serde_json::to_string_pretty(&report)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
        writeln!(stdout, "{text}").map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        render(&report, stdout).map_err(|error| CliFailure::new(1, error.to_string()))?;
    }
    Ok(std::process::ExitCode::SUCCESS)
}

fn render<W: Write>(report: &Report, out: &mut W) -> std::io::Result<()> {
    writeln!(
        out,
        "{}#{} head {} ({}): expected files from run {} ({})",
        report.repo,
        report.pr,
        short(&report.head),
        report.head_ref,
        report.run_id,
        report.artifacts.join(", ")
    )?;
    for file in &report.files {
        let label = match file.change {
            Change::New => "new",
            Change::Modified => "modified",
            Change::Unchanged => "unchanged",
        };
        writeln!(
            out,
            "  {label:<9} {} (+{} -{})",
            file.path, file.added, file.removed
        )?;
    }
    let changed = report
        .files
        .iter()
        .filter(|file| file.change != Change::Unchanged)
        .count();
    match report.outcome {
        Outcome::NothingToApply => {
            writeln!(out, "Nothing to apply: the checkout already matches.")?;
        }
        Outcome::DryRun => writeln!(
            out,
            "Dry run: {changed} file(s) would change. Nothing was written; pass --push to \
             commit and push."
        )?,
        Outcome::Pushed => writeln!(
            out,
            "Committed {} and pushed to {}: {changed} file(s).",
            report.commit.as_deref().map_or("", short),
            report.pushed_to.as_deref().unwrap_or("")
        )?,
    }
    Ok(())
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// The whole command against a GitHub layer and a checkout.
#[allow(
    clippy::too_many_lines,
    reason = "one linear sequence of reads and refusals, kept together so each check's order is visible"
)]
pub(super) fn run(
    gh: &dyn DerivedGitHub,
    git: &Git,
    repo: &str,
    number: u64,
    push: bool,
) -> Result<Report, String> {
    let pr = gh.pull_request(repo, number)?;
    if !pr.state.eq_ignore_ascii_case("open") {
        return Err(format!("#{number} is {}, not open", pr.state));
    }
    if !pr.head_repo.eq_ignore_ascii_case(repo) {
        return Err(format!(
            "#{number}'s head is in {}, not {repo}; fork pull requests are refused",
            pr.head_repo
        ));
    }
    let text = gh
        .base_config(repo, &pr.base_ref)?
        .ok_or_else(|| format!("{} has no .shipyard/config.toml", pr.base_ref))?;
    let config = DerivedFilesConfig::from_config_text(&text);
    if let Some(error) = &config.error {
        return Err(format!("[derived_files] on {}: {error}", pr.base_ref));
    }
    if !config.applies() {
        return Err(format!(
            "{} configures no [derived_files] paths and artifacts",
            pr.base_ref
        ));
    }
    let local = git.head()?;
    if !local.eq_ignore_ascii_case(&pr.head_sha) {
        return Err(format!(
            "the checkout is at {}, but #{number}'s head is {}; check out the pull request's \
             head first",
            short(&local),
            short(&pr.head_sha)
        ));
    }
    git.ensure_clean()?;

    let (run_id, artifacts) = latest_failed_run_with_artifacts(gh, repo, &pr.head_sha, &config)?;
    let files = download_files(gh, repo, &config, &artifacts)?;
    let mut reports = Vec::new();
    let mut changed: Vec<(&str, &[u8])> = Vec::new();
    for (path, data) in &files {
        let current = read_checkout_file(git.dir(), path)?;
        let (change, added, removed) = match &current {
            None => (Change::New, line_count(data), 0),
            Some(old) if old == data => (Change::Unchanged, 0, 0),
            Some(old) => {
                let (added, removed) = line_delta(old, data);
                (Change::Modified, added, removed)
            }
        };
        if change != Change::Unchanged {
            changed.push((path, data));
        }
        reports.push(FileReport {
            path: path.clone(),
            change,
            added,
            removed,
        });
    }
    ensure_head_unmoved(gh, repo, number, &pr.head_sha, "after reading the artifact")?;
    let mut report = Report {
        schema: SCHEMA,
        repo: repo.to_owned(),
        pr: number,
        head: pr.head_sha.clone(),
        head_ref: pr.head_ref.clone(),
        run_id,
        artifacts: artifacts.iter().map(|a| a.name.clone()).collect(),
        outcome: Outcome::DryRun,
        files: reports,
        commit: None,
        pushed_to: None,
    };
    if changed.is_empty() {
        report.outcome = Outcome::NothingToApply;
        return Ok(report);
    }
    if !push {
        return Ok(report);
    }

    for (path, data) in &changed {
        git.refuse_special_tracked(path)?;
        write_checkout_file(git.dir(), path, data)?;
    }
    let mut unexpected: Vec<String> = git
        .changed_paths()?
        .into_iter()
        .filter(|path| !changed.iter().any(|(planned, _)| planned == path))
        .collect();
    unexpected.retain(|path| !path.is_empty());
    if !unexpected.is_empty() {
        return Err(format!(
            "writing the derived files changed paths outside the plan ({}); nothing was \
             committed. Inspect the checkout and restore it with `git checkout -- .`",
            unexpected.join(", ")
        ));
    }
    let paths: Vec<&str> = changed.iter().map(|(path, _)| *path).collect();
    git.add(&paths)?;
    let message = apply_derived::commit_message(run_id, &git.tip_trailers()?, &config);
    let commit = git.commit(&message)?;
    ensure_head_unmoved(gh, repo, number, &pr.head_sha, "before pushing")
        .map_err(|error| format!("{error}. Local commit {} was not pushed.", short(&commit)))?;
    let remote = git.push_remote(repo)?;
    let target = format!("refs/heads/{}", pr.head_ref);
    git.push(&remote, &target)?;
    report.outcome = Outcome::Pushed;
    report.commit = Some(commit);
    report.pushed_to = Some(format!("{remote} {target}"));
    Ok(report)
}

fn ensure_head_unmoved(
    gh: &dyn DerivedGitHub,
    repo: &str,
    number: u64,
    head: &str,
    when: &str,
) -> Result<(), String> {
    let now = gh.pull_request(repo, number)?;
    if now.head_sha.eq_ignore_ascii_case(head) {
        Ok(())
    } else {
        Err(format!(
            "#{number}'s head moved from {} to {} {when}; re-run on the new head",
            short(head),
            short(&now.head_sha)
        ))
    }
}

/// The newest failed run at `head` that carries a configured artifact, and
/// those artifacts.
fn latest_failed_run_with_artifacts(
    gh: &dyn DerivedGitHub,
    repo: &str,
    head: &str,
    config: &DerivedFilesConfig,
) -> Result<(u64, Vec<ArtifactRef>), String> {
    let mut runs: Vec<RunAtHead> = gh
        .runs_at_head(repo, head)?
        .into_iter()
        .filter(|run| run.conclusion.as_deref() == Some("failure"))
        .collect();
    runs.sort_by_key(|run| std::cmp::Reverse(run.id));
    for run in runs {
        let artifacts: Vec<ArtifactRef> = gh
            .run_artifacts(repo, run.id)?
            .into_iter()
            .filter(|artifact| {
                config.mapping(&artifact.name).is_some()
                    && !artifact.expired
                    && artifact
                        .head_sha
                        .as_deref()
                        .is_none_or(|sha| sha.eq_ignore_ascii_case(head))
            })
            .collect();
        if !artifacts.is_empty() {
            return Ok((run.id, artifacts));
        }
    }
    let names: Vec<&str> = config.artifacts.iter().map(|a| a.name.as_str()).collect();
    Err(format!(
        "no failed run at {} carries an unexpired expected-files artifact ({})",
        short(head),
        names.join(", ")
    ))
}

fn download_files(
    gh: &dyn DerivedGitHub,
    repo: &str,
    config: &DerivedFilesConfig,
    artifacts: &[ArtifactRef],
) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let limits = safe_zip::Limits::DEFAULT;
    let mut files = BTreeMap::new();
    let mut folded = std::collections::BTreeSet::new();
    for artifact in artifacts {
        let mapping = config
            .mapping(&artifact.name)
            .ok_or_else(|| format!("artifact `{}` is not configured", artifact.name))?;
        if artifact.size > limits.max_archive_bytes as u64 {
            return Err(format!(
                "artifact `{}` is {} bytes, over the {} byte limit",
                artifact.name, artifact.size, limits.max_archive_bytes
            ));
        }
        let bytes = gh.download_artifact(repo, artifact.id, limits.max_archive_bytes)?;
        let entries = safe_zip::read(&bytes, &limits)
            .map_err(|error| format!("artifact `{}`: {error}", artifact.name))?;
        for entry in entries {
            let path = apply_derived::destination(config, mapping, &entry.name)?;
            if !folded.insert(path.to_lowercase()) {
                return Err(format!("`{path}` appears twice across the artifacts"));
            }
            files.insert(path, entry.data);
        }
    }
    if files.is_empty() {
        return Err("the artifacts hold no files".to_owned());
    }
    Ok(files)
}

fn line_count(data: &[u8]) -> usize {
    data.split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count()
}

/// Lines added and removed, counted as a multiset difference: an
/// approximation of a diffstat that needs no diff.
fn line_delta(old: &[u8], new: &[u8]) -> (usize, usize) {
    let mut counts: BTreeMap<&[u8], isize> = BTreeMap::new();
    for line in new.split(|byte| *byte == b'\n').filter(|l| !l.is_empty()) {
        *counts.entry(line).or_default() += 1;
    }
    for line in old.split(|byte| *byte == b'\n').filter(|l| !l.is_empty()) {
        *counts.entry(line).or_default() -= 1;
    }
    let added = counts
        .values()
        .filter(|n| **n > 0)
        .map(|n| n.unsigned_abs())
        .sum();
    let removed = counts
        .values()
        .filter(|n| **n < 0)
        .map(|n| n.unsigned_abs())
        .sum();
    (added, removed)
}

/// Each existing directory on the way to `rel` must be a real directory,
/// never a symlink or a file.
fn check_parents(root: &Path, rel: &str, create: bool) -> Result<bool, String> {
    let segments: Vec<&str> = rel.split('/').collect();
    let mut dir = root.to_path_buf();
    for segment in &segments[..segments.len().saturating_sub(1)] {
        dir.push(segment);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.file_type().is_dir() => {}
            Ok(_) => {
                return Err(format!(
                    "`{}` in the checkout is a symlink or a file, not a directory",
                    dir.strip_prefix(root).unwrap_or(&dir).display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if !create {
                    return Ok(false);
                }
                std::fs::create_dir(&dir)
                    .map_err(|error| format!("create {}: {error}", dir.display()))?;
            }
            Err(error) => return Err(format!("read {}: {error}", dir.display())),
        }
    }
    Ok(true)
}

/// The checkout's current bytes at `rel`, `None` when absent. Refuses a path
/// that crosses a symlink or is not a regular file.
fn read_checkout_file(root: &Path, rel: &str) -> Result<Option<Vec<u8>>, String> {
    if !check_parents(root, rel, false)? {
        return Ok(None);
    }
    let path = root.join(rel);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => std::fs::read(&path)
            .map(Some)
            .map_err(|error| format!("read {rel}: {error}")),
        Ok(_) => Err(format!("`{rel}` in the checkout is not a regular file")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read {rel}: {error}")),
    }
}

fn write_checkout_file(root: &Path, rel: &str, data: &[u8]) -> Result<(), String> {
    check_parents(root, rel, true)?;
    let path = root.join(rel);
    #[cfg(unix)]
    let mode = match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_file() => {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o777
        }
        Ok(_) => return Err(format!("`{rel}` in the checkout is not a regular file")),
        Err(_) => 0o644,
    };
    #[cfg(not(unix))]
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.file_type().is_file()) {
        return Err(format!("`{rel}` in the checkout is not a regular file"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    atomicwrites::AtomicFile::new(&path, atomicwrites::AllowOverwrite)
        .write_with_options(|file| file.write_all(data), options)
        .map_err(|error| format!("write {rel}: {error}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
            .map_err(|error| format!("chmod {rel}: {error}"))?;
    }
    Ok(())
}

/// `OWNER/REPO` from a GitHub remote URL.
pub(super) fn repo_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))
        .or_else(|| url.strip_prefix("https://github.com/"))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let (owner, name) = rest.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/'))
        .then(|| format!("{owner}/{name}"))
}

/// The checkout, driven through `git` with hooks and fsmonitor disabled.
pub(super) struct Git {
    dir: PathBuf,
}

impl Git {
    pub(super) fn new(dir: &Path) -> Result<Self, CliFailure> {
        let dir = dir
            .canonicalize()
            .map_err(|error| CliFailure::new(2, format!("{}: {error}", dir.display())))?;
        let git = Self { dir };
        let top = git
            .run(&["rev-parse", "--show-toplevel"])
            .map_err(|error| CliFailure::new(2, format!("not a git checkout: {error}")))?;
        let top = PathBuf::from(top.trim());
        let top = top.canonicalize().unwrap_or(top);
        if top != git.dir {
            return Err(CliFailure::new(
                2,
                format!(
                    "run from the checkout's top level ({}), or pass --path",
                    top.display()
                ),
            ));
        }
        Ok(git)
    }

    pub(super) fn dir(&self) -> &Path {
        &self.dir
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .current_dir(&self.dir)
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE");
        command
    }

    fn run(&self, args: &[&str]) -> Result<String, String> {
        self.run_with_input(args, None)
    }

    fn run_with_input(&self, args: &[&str], input: Option<&str>) -> Result<String, String> {
        let mut command = self.command(args);
        command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command
            .spawn()
            .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin
                .write_all(input.as_bytes())
                .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
        }
        let output = child
            .wait_with_output()
            .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "git {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    pub(super) fn head(&self) -> Result<String, String> {
        Ok(self.run(&["rev-parse", "HEAD"])?.trim().to_owned())
    }

    fn status(&self) -> Result<Vec<String>, String> {
        let raw = self.run(&[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ])?;
        let mut paths = Vec::new();
        let mut records = raw.split('\0').filter(|record| !record.is_empty());
        while let Some(record) = records.next() {
            let (code, path) = record.split_at(record.len().min(3));
            paths.push(path.to_owned());
            if code.starts_with('R') || code.starts_with('C') {
                records.next();
            }
        }
        Ok(paths)
    }

    pub(super) fn ensure_clean(&self) -> Result<(), String> {
        let dirty = self.status()?;
        if dirty.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "the checkout is not clean ({}{}); commit or remove local changes first",
                dirty.iter().take(5).cloned().collect::<Vec<_>>().join(", "),
                if dirty.len() > 5 { ", ..." } else { "" }
            ))
        }
    }

    fn changed_paths(&self) -> Result<Vec<String>, String> {
        self.status()
    }

    fn refuse_special_tracked(&self, path: &str) -> Result<(), String> {
        let staged = self.run(&["ls-files", "-s", "--", path])?;
        for line in staged.lines() {
            if line.starts_with("120000 ") || line.starts_with("160000 ") {
                return Err(format!("`{path}` is tracked as a symlink or submodule"));
            }
        }
        Ok(())
    }

    fn add(&self, paths: &[&str]) -> Result<(), String> {
        let mut args = vec!["add", "--"];
        args.extend_from_slice(paths);
        self.run(&args).map(drop)
    }

    fn tip_trailers(&self) -> Result<Vec<String>, String> {
        Ok(self
            .run(&["log", "-1", "--format=%(trailers:only,unfold)"])?
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect())
    }

    fn commit(&self, message: &str) -> Result<String, String> {
        self.run_with_input(
            &["commit", "--no-verify", "--quiet", "-F", "-"],
            Some(message),
        )?;
        self.head()
    }

    pub(super) fn origin_repo(&self) -> Option<String> {
        self.run(&["config", "--get", "remote.origin.url"])
            .ok()
            .and_then(|url| repo_from_url(&url))
    }

    /// The remote whose configured push URL names `repo`. Reads the raw
    /// `remote.<name>.pushurl` (else `.url`), before any `insteadOf`
    /// rewriting, so the match is on what the remote says it is.
    fn push_remote(&self, repo: &str) -> Result<String, String> {
        let remotes = self.run(&["remote"])?;
        let mut names: Vec<&str> = remotes.lines().map(str::trim).collect();
        // Prefer `origin` when several match.
        names.sort_by_key(|name| *name != "origin");
        for name in names {
            let url = self
                .run(&["config", "--get", &format!("remote.{name}.pushurl")])
                .or_else(|_| self.run(&["config", "--get", &format!("remote.{name}.url")]));
            if url
                .ok()
                .and_then(|url| repo_from_url(&url))
                .is_some_and(|found| found.eq_ignore_ascii_case(repo))
            {
                return Ok(name.to_owned());
            }
        }
        Err(format!("no git remote in the checkout pushes to {repo}"))
    }

    fn push(&self, remote: &str, target: &str) -> Result<(), String> {
        let refspec = format!("HEAD:{target}");
        self.run(&["push", "--no-verify", "--quiet", remote, &refspec])
            .map(drop)
    }
}

/// The live GitHub layer.
struct LiveGitHub {
    actions: GitHubActions,
}

impl LiveGitHub {
    fn json(&self, path: &str) -> Result<Value, String> {
        let raw = self
            .actions
            .run_gh(&["api".to_owned(), path.to_owned()])
            .map_err(|error| error.to_string())?;
        serde_json::from_str(&raw).map_err(|error| format!("{path}: malformed JSON: {error}"))
    }
}

fn text(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

impl DerivedGitHub for LiveGitHub {
    fn pull_request(&self, repo: &str, number: u64) -> Result<PrHead, String> {
        let pr = self.json(&format!("repos/{repo}/pulls/{number}"))?;
        let head = PrHead {
            state: text(&pr, "/state"),
            head_sha: text(&pr, "/head/sha"),
            head_ref: text(&pr, "/head/ref"),
            head_repo: text(&pr, "/head/repo/full_name"),
            base_ref: text(&pr, "/base/ref"),
        };
        if head.head_sha.len() != 40 || head.head_ref.is_empty() || head.base_ref.is_empty() {
            return Err(format!(
                "#{number}: pull request read is missing its head or base"
            ));
        }
        Ok(head)
    }

    fn base_config(&self, repo: &str, base: &str) -> Result<Option<String>, String> {
        let path = format!(
            "repos/{repo}/contents/.shipyard/config.toml?ref={}",
            crate::required_check_policy::encode_path_segment(base)
        );
        match self.actions.run_gh(&[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.raw+json".to_owned(),
            path,
        ]) {
            Ok(text) => Ok(Some(text)),
            Err(error) => {
                let detail = error.to_string();
                if detail.contains("404") || detail.contains("Not Found") {
                    Ok(None)
                } else {
                    Err(format!(".shipyard/config.toml on {base}: {detail}"))
                }
            }
        }
    }

    fn runs_at_head(&self, repo: &str, head: &str) -> Result<Vec<RunAtHead>, String> {
        Ok(self
            .actions
            .workflow_runs_for_head_sha(repo, head)
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|run| RunAtHead {
                id: run.id,
                conclusion: run.conclusion,
            })
            .collect())
    }

    fn run_artifacts(&self, repo: &str, run_id: u64) -> Result<Vec<ArtifactRef>, String> {
        let value = self.json(&format!(
            "repos/{repo}/actions/runs/{run_id}/artifacts?per_page=100"
        ))?;
        Ok(parse_artifacts(&value))
    }

    fn download_artifact(
        &self,
        repo: &str,
        artifact_id: u64,
        max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        self.actions
            .run_gh_bytes_bounded(
                &[
                    "api".to_owned(),
                    format!("repos/{repo}/actions/artifacts/{artifact_id}/zip"),
                ],
                DOWNLOAD_TIMEOUT,
                max_bytes,
            )
            .map_err(|error| error.to_string())
    }
}

/// The `artifacts` array of a run's artifact listing.
pub(super) fn parse_artifacts(value: &Value) -> Vec<ArtifactRef> {
    value
        .get("artifacts")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(ArtifactRef {
                        id: item.get("id")?.as_u64()?,
                        name: item.get("name")?.as_str()?.to_owned(),
                        size: item
                            .get("size_in_bytes")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                        expired: item.get("expired").and_then(Value::as_bool).unwrap_or(true),
                        head_sha: item
                            .pointer("/workflow_run/head_sha")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
