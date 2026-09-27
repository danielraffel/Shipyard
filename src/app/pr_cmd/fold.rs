//! `shipyard pr --fold <branch>…`: carry sibling branches' own commits onto
//! the current branch so one pull request ships them together.
//!
//! Only commits the branch adds over `origin/<base>` are taken, merges and
//! version-bump commits are skipped (this pull request computes its own
//! bump), and commits whose patch is already on the current branch are
//! skipped too. The fold is all-or-nothing: a conflict aborts the pick and
//! resets the branch to where it started, which is safe because a clean
//! working tree is required first.

use std::path::Path;
use std::process::Command;

/// Commit subjects that only move version numbers.
const BUMP_SUBJECTS: &[&str] = &["chore: bump versions", "chore(versions): bump"];

/// What folding one branch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Folded {
    pub(super) branch: String,
    pub(super) picked: usize,
    pub(super) skipped_bumps: usize,
    pub(super) skipped_present: usize,
}

fn git(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .map_err(|error| format!("git {}: {error}", args.join(" ")))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The local branch when it exists, else the remote one, fetched now.
fn resolve_branch(cwd: &Path, branch: &str) -> Result<String, String> {
    if git(cwd, &["check-ref-format", "--branch", branch]).is_err() {
        return Err(format!("`{branch}` is not a branch name"));
    }
    let local = format!("refs/heads/{branch}");
    if git(cwd, &["rev-parse", "--verify", "--quiet", &local]).is_ok() {
        return Ok(local);
    }
    let remote = format!("refs/remotes/origin/{branch}");
    git(
        cwd,
        &[
            "fetch",
            "--quiet",
            "origin",
            &format!("+refs/heads/{branch}:{remote}"),
        ],
    )
    .map_err(|error| format!("`{branch}` is neither a local branch nor on origin: {error}"))?;
    Ok(remote)
}

/// Fold every branch in order, or none of them.
pub(super) fn fold_branches(
    cwd: &Path,
    base: &str,
    branches: &[String],
) -> Result<Vec<Folded>, String> {
    let dirty = git(cwd, &["status", "--porcelain", "--untracked-files=no"])?;
    if !dirty.is_empty() {
        return Err(
            "--fold needs a clean working tree; commit or stash tracked changes first".to_owned(),
        );
    }
    let start = git(cwd, &["rev-parse", "HEAD"])?;
    let upstream = format!("origin/{base}");
    let mut results = Vec::new();
    for branch in branches {
        match fold_one(cwd, &upstream, branch) {
            Ok(folded) => results.push(folded),
            Err(error) => {
                let _ = git(cwd, &["cherry-pick", "--abort"]);
                let restored = git(cwd, &["reset", "--hard", "--quiet", &start]);
                return Err(match restored {
                    Ok(_) => format!("{error}; the branch is back where it started"),
                    Err(reset) => {
                        format!("{error}; restoring the starting commit ALSO failed: {reset}")
                    }
                });
            }
        }
    }
    Ok(results)
}

fn fold_one(cwd: &Path, upstream: &str, branch: &str) -> Result<Folded, String> {
    let reference = resolve_branch(cwd, branch)?;
    // `git cherry` lists with `+` each commit of upstream..branch whose patch
    // is not yet on HEAD; anything it leaves out is already here, so a fold
    // repeated after a partial one is a no-op.
    let cherry = git(cwd, &["cherry", "HEAD", &reference, upstream])?;
    let absent: Vec<&str> = cherry
        .lines()
        .filter_map(|line| line.strip_prefix("+ "))
        .collect();
    let ordered = git(
        cwd,
        &[
            "rev-list",
            "--reverse",
            "--no-merges",
            &format!("{upstream}..{reference}"),
        ],
    )?;
    let mut folded = Folded {
        branch: branch.to_owned(),
        picked: 0,
        skipped_bumps: 0,
        skipped_present: ordered
            .lines()
            .filter(|commit| !absent.contains(commit))
            .count(),
    };
    for commit in ordered.lines().filter(|commit| absent.contains(commit)) {
        let subject = git(cwd, &["log", "-1", "--format=%s", commit])?;
        if BUMP_SUBJECTS.iter().any(|bump| subject.starts_with(bump)) {
            folded.skipped_bumps += 1;
            continue;
        }
        git(cwd, &["cherry-pick", "-x", "--allow-empty", commit])
            .map_err(|error| format!("folding `{branch}` stopped at \"{subject}\": {error}"))?;
        folded.picked += 1;
    }
    Ok(folded)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

    fn run(cwd: &Path, args: &[&str]) -> String {
        git(cwd, args).unwrap_or_else(|error| panic!("{error}"))
    }

    fn commit(cwd: &Path, file: &str, text: &str, subject: &str) {
        fs::write(cwd.join(file), text).expect("write");
        run(cwd, &["add", file]);
        run(cwd, &["commit", "--quiet", "-m", subject]);
    }

    /// A clone of a bare origin with `main`; a sibling branch carrying a
    /// commit already on `here`, a feature commit and a bump commit; and the
    /// current branch `here`.
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().expect("tempdir");
        let origin = root.path().join("origin.git");
        let work = root.path().join("work");
        let origin_text = origin.to_str().expect("utf8");
        run(root.path(), &["init", "--quiet", "--bare", origin_text]);
        run(
            root.path(),
            &[
                "clone",
                "--quiet",
                origin_text,
                work.to_str().expect("utf8"),
            ],
        );
        run(&work, &["config", "user.email", "t@example.test"]);
        run(&work, &["config", "user.name", "t"]);
        run(&work, &["config", "commit.gpgsign", "false"]);
        run(&work, &["config", "core.autocrlf", "false"]);
        run(&work, &["checkout", "--quiet", "-b", "main"]);
        commit(&work, "base.txt", "base\n", "base");
        run(&work, &["push", "--quiet", "origin", "main"]);

        run(&work, &["checkout", "--quiet", "-b", "sibling"]);
        commit(&work, "shared.txt", "shared\n", "shared change");
        commit(&work, "sibling.txt", "sibling\n", "sibling feature");
        commit(&work, "VERSION", "2\n", "chore: bump versions");
        run(&work, &["push", "--quiet", "origin", "sibling"]);

        run(&work, &["checkout", "--quiet", "-b", "here", "main"]);
        commit(&work, "shared.txt", "shared\n", "shared change");
        commit(&work, "here.txt", "here\n", "here feature");
        (root, work)
    }

    fn subjects(cwd: &Path) -> Vec<String> {
        run(cwd, &["log", "--format=%s", "origin/main..HEAD"])
            .lines()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn folding_takes_only_the_siblings_own_new_commits() {
        let (_root, work) = repo();
        let folded = fold_branches(&work, "main", &["sibling".to_owned()]).expect("fold");
        assert_eq!(
            folded,
            [Folded {
                branch: "sibling".to_owned(),
                picked: 1,
                skipped_bumps: 1,
                skipped_present: 1,
            }]
        );
        assert_eq!(
            subjects(&work),
            ["sibling feature", "here feature", "shared change"]
        );
        assert!(
            !work.join("VERSION").exists(),
            "the sibling's bump is not carried"
        );

        // Folding again changes nothing.
        let again = fold_branches(&work, "main", &["sibling".to_owned()]).expect("refold");
        assert_eq!(again[0].picked, 0);
        assert_eq!(subjects(&work).len(), 3);
    }

    #[test]
    fn a_branch_only_on_origin_is_fetched_and_folded() {
        let (_root, work) = repo();
        run(&work, &["branch", "-D", "sibling"]);
        let folded = fold_branches(&work, "main", &["sibling".to_owned()]).expect("fold");
        assert_eq!(folded[0].picked, 1);
    }

    #[test]
    fn a_conflict_restores_the_starting_commit() {
        let (_root, work) = repo();
        run(&work, &["checkout", "--quiet", "-b", "clash", "main"]);
        commit(&work, "here.txt", "other\n", "clashing change");
        run(&work, &["checkout", "--quiet", "here"]);
        let start = run(&work, &["rev-parse", "HEAD"]);

        let error = fold_branches(&work, "main", &["sibling".to_owned(), "clash".to_owned()])
            .expect_err("conflict");
        assert!(error.contains("clashing change"), "{error}");
        assert!(error.contains("back where it started"), "{error}");
        assert_eq!(
            run(&work, &["rev-parse", "HEAD"]),
            start,
            "the sibling's pick was undone too"
        );
        assert!(run(&work, &["status", "--porcelain"]).is_empty());
    }

    #[test]
    fn a_dirty_tree_or_unknown_branch_is_refused_untouched() {
        let (_root, work) = repo();
        let start = run(&work, &["rev-parse", "HEAD"]);
        fs::write(work.join("here.txt"), "edited\n").expect("edit");
        let error = fold_branches(&work, "main", &["sibling".to_owned()]).expect_err("dirty");
        assert!(error.contains("clean working tree"), "{error}");
        run(&work, &["checkout", "--quiet", "--", "here.txt"]);

        let error = fold_branches(&work, "main", &["nope".to_owned()]).expect_err("unknown");
        assert!(
            error.contains("neither a local branch nor on origin"),
            "{error}"
        );
        assert_eq!(run(&work, &["rev-parse", "HEAD"]), start);
    }
}
