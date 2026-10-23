//! Binary proof that an inherited `GIT_DIR` cannot make a branch-committed
//! `.shipyard.local/config.toml` look untracked.
//!
//! Git exports `GIT_DIR` to hooks and to `rebase -x`. With it set and no
//! `GIT_WORK_TREE`, git treats the cwd as the top of the work tree, so a
//! tracking check run from inside `.shipyard.local/` would miss the index
//! entry and trust the overlay.

#![cfg(unix)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .expect("git");
    assert!(status.success(), "git {args:?}");
}

/// A repository whose local overlay names an ssh target with a
/// `ProxyCommand` that would create `marker`, committed when `commit_overlay`.
fn repository(root: &Path, marker: &Path, commit_overlay: bool) -> std::path::PathBuf {
    let repo = root.join("repo");
    fs::create_dir_all(repo.join(".shipyard")).expect("project dir");
    fs::create_dir_all(repo.join(".shipyard.local")).expect("local dir");
    git(&repo, &["init", "-q"]);
    fs::write(
        repo.join(".shipyard/config.toml"),
        "[project]\nname = \"probe\"\n",
    )
    .expect("project config");
    fs::write(
        repo.join(".shipyard.local/config.toml"),
        format!(
            "[targets.linux]\nbackend = \"ssh\"\nplatform = \"linux-arm64\"\nhost = \"builder.invalid\"\n\
             ssh_options = [\"-oProxyCommand=/usr/bin/touch {}\"]\n",
            marker.display()
        ),
    )
    .expect("overlay");
    git(&repo, &["add", ".shipyard/config.toml"]);
    if commit_overlay {
        git(&repo, &["add", "-f", ".shipyard.local/config.toml"]);
    }
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

fn targets_list(home: &Path, repo: &Path, git_dir: Option<&Path>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_shipyard"));
    command
        .args(["--json", "--cwd"])
        .arg(repo)
        .args(["targets", "list"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("GIT_WORK_TREE")
        .env_remove("SHIPYARD_TEST_HOME");
    match git_dir {
        Some(git_dir) => command.env("GIT_DIR", git_dir),
        None => command.env_remove("GIT_DIR"),
    };
    command.output().expect("run shipyard")
}

fn describe(output: &Output) -> String {
    format!(
        "status={} stdout={} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn inherited_git_dir_does_not_hide_a_committed_overlay() {
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let marker = temp.path().join("PWNED");
    let repo = repository(temp.path(), &marker, true);
    let git_dir = repo.join(".git");

    for inherited in [None, Some(git_dir.as_path())] {
        let output = targets_list(&home, &repo, inherited);
        assert!(
            !output.status.success(),
            "GIT_DIR={inherited:?} must refuse: {}",
            describe(&output)
        );
        let text = describe(&output);
        assert!(text.contains("repository-controlled"), "{text}");
    }
    assert!(!marker.exists(), "ProxyCommand ran");
}

#[test]
fn inherited_git_dir_still_honours_an_untracked_overlay() {
    // Positive control: the same overlay, uncommitted, is the operator's own.
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let marker = temp.path().join("PWNED");
    let repo = repository(temp.path(), &marker, false);
    let output = targets_list(&home, &repo, Some(&repo.join(".git")));
    assert!(output.status.success(), "{}", describe(&output));
}
