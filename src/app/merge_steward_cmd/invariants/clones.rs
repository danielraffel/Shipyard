//! The fleet-level `config_clone` invariant: every repository in the fleet
//! list has a steward config clone with a readable `.shipyard/config.toml`.
//!
//! The steward tick runs each repository's pass from a config-only clone. A
//! row added to `fleet-repos.txt` without its clone is skipped on every tick
//! with one log line and no pass at all (fleet-ssh, 2026-11-12), so nothing
//! about that repository is admitted, refused or reported. This check runs
//! once at tick start. With `--heal` it clones a missing repository the way
//! the existing clones are cloned (the same remote form, so SSH stays SSH),
//! sparse to `.shipyard` and `.github/workflows`, and adds
//! `.github/workflows` to an existing sparse clone that lacks it, so the
//! repository invariants read workflows from disk instead of the API. The
//! clones are read-only config; healing never writes to a remote. Every
//! heal is idempotent: a second run finds nothing to do.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde::Serialize;

use super::Invariant;
use crate::app::CliFailure;

/// The fleet-level invariant id.
pub(crate) const CONFIG_CLONE: &str = "config_clone";
/// What a healed clone checks out.
const SPARSE_PATHS: [&str; 2] = [".shipyard", ".github/workflows"];

/// `shipyard runner steward-invariants` arguments.
pub(crate) struct CloneArgs {
    pub fleet_repos: PathBuf,
    pub clones: PathBuf,
    pub heal: bool,
}

/// One fleet row's result.
#[derive(Debug, Serialize)]
pub(crate) struct RepoInvariant {
    pub repo: String,
    #[serde(flatten)]
    pub invariant: Invariant,
}

/// Repositories in a fleet list: one `OWNER/REPO` per line, `#` comments.
pub(crate) fn fleet_repos(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The clone directory the tick uses for `repo`: its name under `clones`.
pub(crate) fn clone_dir(clones: &Path, repo: &str) -> PathBuf {
    clones.join(repo.rsplit('/').next().unwrap_or(repo))
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// How the existing clones name their remotes: the text before `OWNER/REPO`
/// and the suffix after it, e.g. `git@github.com:` and `.git`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RemoteForm {
    pub prefix: String,
    pub suffix: String,
}

impl Default for RemoteForm {
    fn default() -> Self {
        Self {
            prefix: "git@github.com:".to_owned(),
            suffix: ".git".to_owned(),
        }
    }
}

impl RemoteForm {
    /// The form of `url`, a remote of `repo`; `None` when it does not end in
    /// `repo`.
    pub(crate) fn of(url: &str, repo: &str) -> Option<Self> {
        let trimmed = url.trim().trim_end_matches('/');
        let (body, suffix) = trimmed
            .strip_suffix(".git")
            .map_or((trimmed, ""), |body| (body, ".git"));
        let split = body.len().checked_sub(repo.len())?;
        if !body.is_char_boundary(split) || !body[split..].eq_ignore_ascii_case(repo) {
            return None;
        }
        let prefix = &body[..split];
        (prefix.ends_with('/') || prefix.ends_with(':')).then(|| Self {
            prefix: prefix.to_owned(),
            suffix: suffix.to_owned(),
        })
    }

    pub(crate) fn url(&self, repo: &str) -> String {
        format!("{}{repo}{}", self.prefix, self.suffix)
    }
}

/// The remote form the first existing clone uses, else SSH to GitHub.
fn remote_form(clones: &Path, repos: &[String]) -> RemoteForm {
    repos
        .iter()
        .filter(|repo| clone_dir(clones, repo).join(".git").exists())
        .find_map(|repo| {
            let url = git(&clone_dir(clones, repo), &["remote", "get-url", "origin"]).ok()?;
            RemoteForm::of(&url, repo)
        })
        .unwrap_or_default()
}

/// Check, and with `heal` repair, every fleet row's clone.
pub(crate) fn check(args: &CloneArgs) -> Result<Vec<RepoInvariant>, String> {
    let text = std::fs::read_to_string(&args.fleet_repos)
        .map_err(|error| format!("{}: {error}", args.fleet_repos.display()))?;
    let repos = fleet_repos(&text);
    let form = remote_form(&args.clones, &repos);
    Ok(repos
        .iter()
        .map(|repo| RepoInvariant {
            repo: repo.clone(),
            invariant: check_one(&args.clones, repo, &form, args.heal),
        })
        .collect())
}

fn check_one(clones: &Path, repo: &str, form: &RemoteForm, heal: bool) -> Invariant {
    let dir = clone_dir(clones, repo);
    let mut healed = Vec::new();
    if !dir.join(".git").exists() {
        let url = form.url(repo);
        if !heal {
            return Invariant::fails(
                CONFIG_CLONE,
                format!("no steward config clone at {}", dir.display()),
                format!(
                    "clone {url} into {} (sparse: {}), or run the tick with healing on",
                    dir.display(),
                    SPARSE_PATHS.join(" ")
                ),
            );
        }
        if let Err(error) = clone(&url, &dir) {
            return Invariant::fails(
                CONFIG_CLONE,
                format!(
                    "no steward config clone at {}; cloning failed: {error}",
                    dir.display()
                ),
                format!(
                    "clone {url} into {} by hand (sparse: {}) and check this host's access to it",
                    dir.display(),
                    SPARSE_PATHS.join(" ")
                ),
            );
        }
        healed.push(format!(
            "cloned {url} into {} (sparse: {})",
            dir.display(),
            SPARSE_PATHS.join(" ")
        ));
    } else if heal && let Some(step) = widen_sparse(&dir) {
        match step {
            Ok(done) => healed.push(done),
            Err(error) => healed.push(format!("not healed: {error}")),
        }
    }
    let config = dir.join(".shipyard/config.toml");
    let mut invariant = match std::fs::read_to_string(&config) {
        Err(error) => Invariant::fails(
            CONFIG_CLONE,
            format!("{} is unreadable: {error}", config.display()),
            format!(
                "add .shipyard/config.toml to {repo}'s default branch with the fleet's \
                 [auto_merge] table (reviewer_signers, reviewer_stamp_sessions)"
            ),
        ),
        Ok(text) => match text.parse::<toml::Table>() {
            Err(error) => Invariant::fails(
                CONFIG_CLONE,
                format!("{} does not parse: {error}", config.display()),
                format!("fix .shipyard/config.toml on {repo}'s default branch"),
            ),
            Ok(_) => Invariant::holds(CONFIG_CLONE, format!("{} is readable", config.display())),
        },
    };
    invariant.healed = healed;
    invariant
}

fn clone(url: &str, dir: &Path) -> Result<(), String> {
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let output = Command::new("git")
        .args([
            "clone",
            "--quiet",
            "--depth",
            "1",
            "--filter=blob:none",
            "--sparse",
            url,
        ])
        .arg(dir)
        .output()
        .map_err(|error| format!("git clone: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git clone: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let mut args = vec!["sparse-checkout", "set"];
    args.extend(SPARSE_PATHS);
    git(dir, &args).map(|_| ())
}

/// A sparse clone that does not check out `.github/workflows` gets it added;
/// `None` when there is nothing to do.
fn widen_sparse(dir: &Path) -> Option<Result<String, String>> {
    if git(dir, &["config", "--bool", "core.sparseCheckout"])
        .ok()?
        .as_str()
        != "true"
    {
        return None;
    }
    let listed = git(dir, &["sparse-checkout", "list"]).ok()?;
    if listed
        .lines()
        .any(|line| line.trim() == ".github/workflows" || line.trim() == ".github")
    {
        return None;
    }
    Some(
        git(dir, &["sparse-checkout", "add", ".github/workflows"]).map(|_| {
            format!(
                "added .github/workflows to the sparse checkout of {}",
                dir.display()
            )
        }),
    )
}

/// `ok`, or `FAIL:config_clone@<repo>,...` for the tick's summary line.
#[must_use]
pub(crate) fn summary_token(results: &[RepoInvariant]) -> String {
    let failed: Vec<String> = results
        .iter()
        .filter(|result| result.invariant.incident())
        .map(|result| format!("{}@{}", result.invariant.id, result.repo))
        .collect();
    if failed.is_empty() {
        "ok".to_owned()
    } else {
        format!("FAIL:{}", failed.join(","))
    }
}

/// `shipyard runner steward-invariants`.
///
/// # Errors
///
/// When the fleet list cannot be read or the report cannot be written.
pub(crate) fn command<W: Write>(
    args: &CloneArgs,
    json: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let results = check(args).map_err(|error| CliFailure::new(2, error))?;
    let summary = summary_token(&results);
    if json {
        let mut data = std::collections::BTreeMap::new();
        data.insert(
            "summary".to_owned(),
            serde_json::Value::from(summary.clone()),
        );
        data.insert(
            "invariants".to_owned(),
            serde_json::to_value(&results)
                .map_err(|error| CliFailure::new(1, error.to_string()))?,
        );
        crate::output::write_json_envelope(stdout, "runner.steward-invariants", data)
            .map_err(|error| CliFailure::new(1, error.to_string()))?;
    } else {
        let io = |error: std::io::Error| CliFailure::new(1, error.to_string());
        writeln!(stdout, "invariants={summary}").map_err(io)?;
        for result in &results {
            let invariant = &result.invariant;
            for heal in &invariant.healed {
                writeln!(stdout, "{} healed: {heal}", result.repo).map_err(io)?;
            }
            if invariant.incident() {
                writeln!(
                    stdout,
                    "{} INCIDENT {}: {} fix: {}",
                    result.repo,
                    invariant.id,
                    invariant.detail,
                    invariant.fix.as_deref().unwrap_or_default()
                )
                .map_err(io)?;
            }
        }
    }
    Ok(if summary == "ok" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
