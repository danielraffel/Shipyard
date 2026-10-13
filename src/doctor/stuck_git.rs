//! Git operations left in progress in a primary checkout.
//!
//! A cherry-pick, merge, revert, rebase or `git am` that stops on a conflict
//! leaves its state in the checkout's `.git` directory until someone finishes
//! or aborts it. Nothing else notices: every later `git` command in that
//! checkout runs against the half-applied state. This check reports any such
//! operation older than [`STALE_AFTER`] in the primary checkouts it examines,
//! with the commands that back the state up and then abort it. It only
//! reports: it never aborts anything itself.
//!
//! The checkouts examined are the primary checkout of the repository doctor
//! runs in, its siblings named in [`FLEET_SIBLINGS`] when they exist, and every
//! path in `doctor.git_checkouts`. Linked worktrees keep their own state
//! elsewhere and are not examined.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use super::DoctorEntry;
use crate::config::LoadedConfig;

/// An operation is reported once its state is older than this.
pub const STALE_AFTER: Duration = Duration::from_hours(1);
/// Config key listing more checkouts to examine.
pub const CHECKOUTS_KEY: &str = "doctor.git_checkouts";
/// Sibling checkouts of the current repository's primary checkout examined
/// when they exist: the repositories one fleet works across.
pub const FLEET_SIBLINGS: [&str; 4] = ["pulp", "Shipyard", "tartci", "vellum"];

/// The kind of operation left in progress.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Operation {
    /// `CHERRY_PICK_HEAD`.
    CherryPick,
    /// `MERGE_HEAD`.
    Merge,
    /// `REVERT_HEAD`.
    Revert,
    /// `rebase-merge/` or `rebase-apply/` (not `git am`).
    Rebase,
    /// `rebase-apply/applying`.
    Am,
    /// Conflicted index entries with no operation marker.
    UnmergedEntries,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::CherryPick => "cherry-pick",
            Self::Merge => "merge",
            Self::Revert => "revert",
            Self::Rebase => "rebase",
            Self::Am => "am",
            Self::UnmergedEntries => "unmerged index entries",
        }
    }

    /// The command that abandons the operation and restores the state before it.
    fn abort(self) -> &'static str {
        match self {
            Self::CherryPick => "cherry-pick --abort",
            Self::Merge => "merge --abort",
            Self::Revert => "revert --abort",
            Self::Rebase => "rebase --abort",
            Self::Am => "am --abort",
            Self::UnmergedEntries => "reset --merge",
        }
    }
}

/// One operation found in progress.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    /// What is in progress.
    pub operation: Operation,
    /// The marker it was recognised by, relative to the git directory.
    pub marker: String,
    /// How long ago the marker was written.
    pub age: Duration,
    /// Conflicted paths in the index.
    pub unmerged: Vec<String>,
}

/// What examining one checkout found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckoutState {
    /// No operation in progress older than the threshold.
    Clean {
        /// Operations in progress but younger than the threshold.
        recent: usize,
    },
    /// Operations in progress past the threshold.
    Stuck(Vec<Finding>),
    /// The checkout could not be read; it is not reported clean.
    Unreadable(String),
}

/// Read the operations in progress in the primary checkout at `checkout`.
///
/// `unmerged` lists the conflicted paths of its index (`git ls-files -u`), so
/// tests can drive it without a conflicted repository.
pub fn examine(
    checkout: &Path,
    now: SystemTime,
    unmerged: impl FnOnce(&Path) -> Result<Vec<String>, String>,
) -> CheckoutState {
    let git_dir = checkout.join(".git");
    if !git_dir.is_dir() {
        return CheckoutState::Unreadable(format!(
            "{} is not a primary checkout (no .git directory)",
            git_dir.display()
        ));
    }
    let unmerged = match unmerged(checkout) {
        Ok(paths) => paths,
        Err(error) => return CheckoutState::Unreadable(error),
    };
    let age = |relative: &str| -> Option<Result<Duration, String>> {
        let path = git_dir.join(relative);
        let metadata = fs::symlink_metadata(&path).ok()?;
        Some(
            metadata
                .modified()
                .map(|modified| now.duration_since(modified).unwrap_or_default())
                .map_err(|error| format!("cannot read the time of {}: {error}", path.display())),
        )
    };
    let mut found = Vec::new();
    let mut markers = vec![
        (Operation::CherryPick, "CHERRY_PICK_HEAD"),
        (Operation::Merge, "MERGE_HEAD"),
        (Operation::Revert, "REVERT_HEAD"),
        (Operation::Rebase, "rebase-merge"),
    ];
    if git_dir.join("rebase-apply/applying").exists() {
        markers.push((Operation::Am, "rebase-apply"));
    } else {
        markers.push((Operation::Rebase, "rebase-apply"));
    }
    for (operation, marker) in markers {
        match age(marker) {
            Some(Ok(age)) => found.push((operation, marker.to_owned(), age)),
            Some(Err(error)) => return CheckoutState::Unreadable(error),
            None => {}
        }
    }
    if found.is_empty() && !unmerged.is_empty() {
        // Conflicts with no operation marker: the index was last written
        // when they were recorded.
        match age("index") {
            Some(Ok(age)) => found.push((Operation::UnmergedEntries, "index".to_owned(), age)),
            Some(Err(error)) => return CheckoutState::Unreadable(error),
            None => {
                return CheckoutState::Unreadable(format!(
                    "{} lists unmerged entries but has no index",
                    checkout.display()
                ));
            }
        }
    }
    let total = found.len();
    let stuck: Vec<Finding> = found
        .into_iter()
        .filter(|(_, _, age)| *age >= STALE_AFTER)
        .map(|(operation, marker, age)| Finding {
            operation,
            marker,
            age,
            unmerged: unmerged.clone(),
        })
        .collect();
    if stuck.is_empty() {
        CheckoutState::Clean { recent: total }
    } else {
        CheckoutState::Stuck(stuck)
    }
}

/// The commands that back up `finding`'s state in `checkout` and then abort
/// it, in order. The backup keeps the current `HEAD` on a branch, the working
/// tree's changes against it as a patch, and the operation's own state
/// (todo list, message, the commit being applied) as a copy, so aborting
/// loses nothing that cannot be put back.
#[must_use]
pub fn remedy(checkout: &Path, finding: &Finding, stamp: &str) -> Vec<String> {
    let path = shell_quote(&checkout.display().to_string());
    let backup = format!(
        "shipyard-backup/{}-{stamp}",
        finding.operation.name().replace(' ', "-")
    );
    let backup_dir = format!(
        "\"$HOME/.shipyard-git-backups/{}-{stamp}\"",
        checkout
            .file_name()
            .map_or_else(|| "checkout".into(), |name| name.to_string_lossy())
    );
    let mut commands = vec![
        format!("mkdir -p {backup_dir}"),
        format!("git -C {path} branch {backup} HEAD"),
        format!("git -C {path} diff HEAD > {backup_dir}/worktree.patch"),
        format!("git -C {path} status --porcelain=v2 > {backup_dir}/status.txt"),
    ];
    if finding.operation != Operation::UnmergedEntries {
        commands.push(format!(
            "cp -Rp {path}/.git/{} {backup_dir}/",
            finding.marker
        ));
        for extra in ["MERGE_MSG", "ORIG_HEAD", "AUTO_MERGE"] {
            commands.push(format!(
                "[ -e {path}/.git/{extra} ] && cp -p {path}/.git/{extra} {backup_dir}/"
            ));
        }
    }
    commands.push(format!("git -C {path} {}", finding.operation.abort()));
    commands
}

fn shell_quote(text: &str) -> String {
    if text
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || "/._-+:@".contains(ch))
    {
        text.to_owned()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

fn hours(age: Duration) -> String {
    let minutes = age.as_secs() / 60;
    if minutes >= 48 * 60 {
        format!("{} days", minutes / (24 * 60))
    } else {
        format!("{}h{:02}m", minutes / 60, minutes % 60)
    }
}

/// The doctor entry for one checkout.
#[must_use]
pub fn entry(checkout: &Path, state: &CheckoutState, stamp: &str) -> DoctorEntry {
    match state {
        CheckoutState::Clean { recent } => DoctorEntry {
            ok: true,
            version: Some(if *recent == 0 {
                "no git operation in progress".to_owned()
            } else {
                format!("{recent} operation(s) in progress for under an hour")
            }),
            detail: None,
            error: None,
        },
        CheckoutState::Unreadable(error) => DoctorEntry {
            ok: false,
            version: None,
            detail: Some(format!("{error}; not reported as clean")),
            error: Some("unreadable".to_owned()),
        },
        CheckoutState::Stuck(findings) => {
            let mut detail = String::new();
            for finding in findings {
                let _ = writeln!(
                    detail,
                    "{} in progress for {} ({} in {}/.git)",
                    finding.operation.name(),
                    hours(finding.age),
                    finding.marker,
                    checkout.display()
                );
                if !finding.unmerged.is_empty() {
                    let _ = writeln!(detail, "unmerged: {}", finding.unmerged.join(", "));
                }
                detail.push_str("back up, then abort (shipyard never aborts it for you):\n");
                for command in remedy(checkout, finding, stamp) {
                    let _ = writeln!(detail, "  {command}");
                }
            }
            let names: BTreeSet<&str> = findings
                .iter()
                .map(|finding| finding.operation.name())
                .collect();
            DoctorEntry {
                ok: false,
                version: None,
                detail: Some(detail),
                error: Some(format!(
                    "{} left in progress",
                    names.into_iter().collect::<Vec<_>>().join(", ")
                )),
            }
        }
    }
}

/// `git ls-files -u` in `checkout`: the conflicted paths, once each.
///
/// # Errors
///
/// When git cannot be run or fails.
pub fn git_unmerged(checkout: &Path) -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(checkout)
        .args(["ls-files", "-u", "-z"])
        .output()
        .map_err(|error| format!("cannot run git in {}: {error}", checkout.display()))?;
    if !output.status.success() {
        return Err(format!(
            "git ls-files -u failed in {}: {}",
            checkout.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let paths: BTreeSet<String> = output
        .stdout
        .split(|byte| *byte == 0)
        .filter_map(|record| {
            let record = String::from_utf8_lossy(record);
            record.split_once('\t').map(|(_, path)| path.to_owned())
        })
        .collect();
    Ok(paths.into_iter().collect())
}

/// The primary checkout of the repository containing `cwd`.
fn primary_checkout(cwd: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    (common.file_name()? == ".git").then(|| common.parent().map(Path::to_path_buf))?
}

fn expand_home(text: &str, home: &Path) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if text == "~" => home.to_path_buf(),
        None => PathBuf::from(text),
    }
}

/// The checkouts to examine: the current repository's primary checkout, its
/// [`FLEET_SIBLINGS`] that exist, and every configured path (examined even
/// when missing, so a wrong path is reported rather than skipped).
#[must_use]
pub fn checkouts(cwd: &Path, configured: &[String], home: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut add = |path: PathBuf| {
        let key = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key) {
            out.push(path);
        }
    };
    if let Some(primary) = primary_checkout(cwd) {
        if let Some(parent) = primary.parent() {
            for sibling in FLEET_SIBLINGS {
                let candidate = parent.join(sibling);
                if candidate.join(".git").is_dir() {
                    add(candidate);
                }
            }
        }
        add(primary);
    }
    for text in configured {
        add(expand_home(text, home));
    }
    out
}

/// `doctor.git_checkouts` from config: a list of paths.
#[must_use]
pub fn configured_checkouts(config: Option<&LoadedConfig>) -> Vec<String> {
    config
        .and_then(|config| config.get(CHECKOUTS_KEY))
        .and_then(toml::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// The doctor section: one entry per checkout examined, keyed by its path.
/// With no checkout to examine it says so instead of reporting nothing.
#[must_use]
pub fn doctor_section(
    checkouts: &[PathBuf],
    now: SystemTime,
    stamp: &str,
) -> BTreeMap<String, DoctorEntry> {
    let mut section = BTreeMap::new();
    if checkouts.is_empty() {
        section.insert(
            "checkouts".to_owned(),
            DoctorEntry {
                ok: false,
                version: None,
                detail: Some(format!(
                    "not inside a git repository and no `{CHECKOUTS_KEY}` configured, so no \
                     checkout was examined"
                )),
                error: Some("none examined".to_owned()),
            },
        );
        return section;
    }
    for checkout in checkouts {
        let state = examine(checkout, now, git_unmerged);
        section.insert(
            checkout.display().to_string(),
            entry(checkout, &state, stamp),
        );
    }
    section
}

#[cfg(test)]
mod tests;
