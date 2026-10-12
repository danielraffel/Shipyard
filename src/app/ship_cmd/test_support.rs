use std::process::Stdio;

use toml::Table;

use crate::config::{LoadedConfig, LocalOverlaySource};

pub(super) fn git(args: &[&str], cwd: &std::path::Path) {
    let status = crate::supervised::git_supervised()
        .args(args)
        .current_dir(cwd)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@t")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("git command should run");
    assert!(status.success(), "git command failed: {args:?}");
}

/// Capture a git command's trimmed stdout (e.g. `rev-parse HEAD`) so a
/// test can pin the issue #321 merge preflight's live-head snapshot to
/// the seeded repo's real HEAD SHA.
pub(super) fn git_capture(args: &[&str], cwd: &std::path::Path) -> String {
    let output = crate::supervised::git_supervised()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command should run");
    assert!(output.status.success(), "git command failed: {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

pub(super) fn seed_repo(repo: &std::path::Path) {
    std::fs::create_dir_all(repo).expect("repo dir");
    git(&["init", "--quiet", "--initial-branch=main"], repo);
    std::fs::write(repo.join("README.md"), "seed\n").expect("readme");
    git(&["add", "."], repo);
    git(&["commit", "-q", "-m", "seed"], repo);
    git(
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/danielraffel/pulp.git",
        ],
        repo,
    );
    git(&["checkout", "-q", "-b", "feature/test"], repo);
    refuse_gh_in(repo);
}

/// Give `repo` a Shipyard project config that runs a refusing fake `gh`, for
/// the commands that load their config from the checkout rather than from the
/// one the test passes. It is excluded from git, so the tree stays clean.
fn refuse_gh_in(repo: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::io::Write as _;
        std::fs::create_dir_all(repo.join(".shipyard")).expect("project config dir");
        std::fs::write(
            repo.join(".shipyard/config.toml"),
            crate::test_support::refusing_gh_config(),
        )
        .expect("project config");
        std::fs::create_dir_all(repo.join(".git/info")).expect("git info dir");
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(repo.join(".git/info/exclude"))
            .and_then(|mut exclude| exclude.write_all(b"/.shipyard/\n"))
            .expect("exclude the project config");
    }
    #[cfg(not(unix))]
    let _ = repo;
}

#[cfg(unix)]
pub(super) fn seed_repo_with_local_origin(repo: &std::path::Path, remote: &std::path::Path) {
    std::fs::create_dir_all(repo).expect("repo dir");
    std::fs::create_dir_all(remote).expect("remote dir");
    git(&["init", "--quiet", "--bare"], remote);
    git(&["init", "--quiet", "--initial-branch=main"], repo);
    std::fs::write(repo.join("README.md"), "seed\n").expect("readme");
    git(&["add", "."], repo);
    git(&["commit", "-q", "-m", "Seed repo"], repo);
    git(
        &["remote", "add", "origin", remote.to_str().expect("remote")],
        repo,
    );
    git(&["push", "-u", "origin", "main"], repo);
    git(&["checkout", "-q", "-b", "feature/test"], repo);
    refuse_gh_in(repo);
}

#[cfg(unix)]
pub(super) fn fake_gh(path: &std::path::Path, script_body: &str) {
    crate::test_support::write_executable_script(path, &format!("#!/bin/sh\n{script_body}\n"));
}

pub(super) fn loaded_config(root: &std::path::Path) -> LoadedConfig {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut data = r#"
        [validation.default]
        command = "rustc --version"

        [targets.mac]
        backend = "local"
        platform = "macos-arm64"
    "#
    .to_owned();
    // Every GitHub client runs a refusing fake `gh` rather than the host's.
    #[cfg(unix)]
    data.push_str(&crate::test_support::refusing_gh_config());
    let data = data.parse::<Table>().expect("config TOML");
    LoadedConfig {
        data,
        global_dir: root.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

pub(super) fn unreachable_ssh_config(root: &std::path::Path) -> LoadedConfig {
    let data = r#"
        [validation.default]
        command = "make test"

        [targets.linux]
        backend = "ssh"
        platform = "linux-x64"
        repo_path = "~/repo"
    "#
    .parse::<Table>()
    .expect("config TOML");
    LoadedConfig {
        data,
        global_dir: root.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}

pub(super) fn local_and_unreachable_config(root: &std::path::Path) -> LoadedConfig {
    let data = r#"
        [validation.default]
        command = "rustc --version"

        [targets.mac]
        backend = "local"
        platform = "macos-arm64"

        [targets.linux]
        backend = "ssh"
        platform = "linux-x64"
        repo_path = "~/repo"
    "#
    .parse::<Table>()
    .expect("config TOML");
    LoadedConfig {
        data,
        global_dir: root.join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    }
}
