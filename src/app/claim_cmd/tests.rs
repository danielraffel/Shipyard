use super::*;

fn script(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(name);
    crate::test_support::write_executable_script(&path, &format!("#!/bin/sh\n{body}\n"));
    path.to_string_lossy().into_owned()
}

#[test]
#[cfg(unix)]
fn a_ghread_timeout_is_not_retried_through_ghapp() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("ghapp.log");
    let ghread = script(temp.path(), "ghread", "sleep 5");
    let ghapp = script(
        temp.path(),
        "ghapp",
        &format!("echo called >> '{}'; echo ok", log.display()),
    );
    let args = vec!["api".to_owned(), "x".to_owned()];
    let error = fleet_gh_with(
        "Generous-Corp/vellum",
        &args,
        Duration::from_millis(300),
        &ghread,
        &ghapp,
    )
    .expect_err("timed out");
    assert!(error.contains("timed out"), "{error}");
    assert!(!log.exists(), "ghapp ran after a ghread timeout");
}

#[test]
#[cfg(unix)]
fn ghapp_reads_what_ghread_does_not_serve() {
    let temp = tempfile::tempdir().expect("tempdir");
    let ghread = script(temp.path(), "ghread", "exit 75");
    let ghapp = script(temp.path(), "ghapp", "echo from-ghapp");
    let missing = temp.path().join("absent").to_string_lossy().into_owned();
    let args = vec!["api".to_owned(), "x".to_owned()];
    for first in [&ghread, &missing] {
        let out = fleet_gh_with(
            "Generous-Corp/vellum",
            &args,
            Duration::from_secs(5),
            first,
            &ghapp,
        )
        .expect("fallback");
        assert_eq!(out.trim(), "from-ghapp");
    }
}

#[test]
#[cfg(unix)]
fn shipyard_pr_hands_its_push_the_head_it_checked() {
    let temp = tempfile::tempdir().expect("tempdir");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).expect("repo");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .current_dir(&repo)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "x"]);
    git(&[
        "remote",
        "add",
        "origin",
        "git@github.com:Generous-Corp/vellum.git",
    ]);
    let gh = temp.path().join("gh");
    script(temp.path(), "gh", "exit 0");
    TEST_FENCE_ON.with(|on| on.set(true));
    let env = fence_before_push(&repo, "main", Some(&gh)).expect("allowed");
    TEST_FENCE_ON.with(|on| on.set(false));
    let head = head_sha(&repo).expect("head");
    assert_eq!(
        env,
        vec![(OsString::from(DONE_ENV), OsString::from(head.clone()))]
    );
    let stdin = format!(
        "refs/heads/main {head} refs/heads/main {}\n",
        "0".repeat(40)
    );
    assert!(already_checked(&stdin, Some(&head)));
    assert!(
        fence_before_push(&repo, "main", Some(&gh))
            .expect("off")
            .is_empty(),
        "nothing handed on when the fence is off"
    );
}

#[test]
fn a_remote_that_is_not_github_is_named_as_not_checked() {
    for url in [
        "m3-git:Generous-Corp/vellum.git",
        "git@gihtub.com:o/r.git",
        "/srv/git/r.git",
    ] {
        assert_eq!(
            github_target(url),
            Err(format!(
                "claim-fence: {url} is not a GitHub remote; not checked"
            ))
        );
    }
    assert_eq!(
        github_target("git@github.com:Generous-Corp/vellum.git").as_deref(),
        Ok("Generous-Corp/vellum")
    );
}

#[test]
#[cfg(unix)]
fn a_claim_record_stays_in_the_worktree_that_wrote_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let main = temp.path().join("main");
    let lane = temp.path().join("lane");
    std::fs::create_dir_all(&main).expect("main");
    let git = |dir: &Path, args: &[&str]| {
        let status = git_here(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args(args)
            .status()
            .expect("git");
        assert!(status.success(), "git {args:?}");
    };
    git(&main, &["init", "-q", "-b", "main"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            &lane.to_string_lossy(),
            "-b",
            "lane",
        ],
    );
    let record = |pr: u64| ClaimRecord {
        repo: "generous-corp/vellum".to_owned(),
        pr,
        owner: "lane-a".to_owned(),
        until: "2026-11-27T22:00:00Z".to_owned(),
        comment_id: 10,
    };
    write_record(&lane, &record(462)).expect("write");
    assert_eq!(read_records(&lane), vec![record(462)]);
    assert!(
        read_records(&main).is_empty(),
        "leaked into the main worktree"
    );

    write_record(&main, &record(463)).expect("write");
    assert_eq!(read_records(&main), vec![record(463)]);
    assert_eq!(
        read_records(&lane),
        vec![record(462)],
        "leaked into the lane"
    );

    // A record in the clone's shared config is not this worktree's.
    git(
        &main,
        &[
            "config",
            "--local",
            &ClaimRecord::key("generous-corp/vellum", 464),
            &record(464).value(),
        ],
    );
    assert_eq!(read_records(&lane), vec![record(462)]);
    assert_eq!(read_records(&main), vec![record(463)]);

    remove_record(&lane, "Generous-Corp/vellum", 462).expect("remove");
    assert!(read_records(&lane).is_empty());
    assert_eq!(read_records(&main), vec![record(463)]);
}

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    git_here(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .expect("git")
}

fn git_must(dir: &Path, args: &[&str]) -> String {
    let output = git_out(dir, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn a_record(pr: u64) -> ClaimRecord {
    ClaimRecord {
        repo: "generous-corp/vellum".to_owned(),
        pr,
        owner: "lane-a".to_owned(),
        until: "2026-11-27T22:00:00Z".to_owned(),
        comment_id: 10,
    }
}

#[test]
#[cfg(unix)]
fn recording_a_claim_keeps_a_bare_clone_s_linked_worktree_working() {
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    let bare = temp.path().join("bare.git");
    let lane = temp.path().join("lane");
    std::fs::create_dir_all(&source).expect("source");
    git_must(&source, &["init", "-q", "-b", "main"]);
    git_must(&source, &["commit", "-q", "--allow-empty", "-m", "x"]);
    git_must(
        temp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            &source.to_string_lossy(),
            &bare.to_string_lossy(),
        ],
    );
    git_must(
        &bare,
        &[
            "worktree",
            "add",
            "-q",
            &lane.to_string_lossy(),
            "-b",
            "lane",
        ],
    );
    git_must(&lane, &["status", "--short"]);

    write_record(&lane, &a_record(462)).expect("write");
    assert_eq!(read_records(&lane), vec![a_record(462)]);
    git_must(&lane, &["status", "--short"]);
    assert_eq!(
        git_must(&lane, &["rev-parse", "--is-bare-repository"]),
        "false"
    );
    assert_eq!(
        git_must(&bare, &["rev-parse", "--is-bare-repository"]),
        "true"
    );
}

#[test]
#[cfg(unix)]
fn recording_a_claim_keeps_a_shared_core_worktree_on_the_main_worktree_only() {
    let temp = tempfile::tempdir().expect("tempdir");
    let main = temp.path().join("main");
    let gitdir = temp.path().join("repo.git");
    let lane = temp.path().join("lane");
    std::fs::create_dir_all(&main).expect("main");
    git_must(
        &main,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            &gitdir.to_string_lossy(),
        ],
    );
    git_must(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
    // A git dir that names its work tree in its shared config (a submodule's
    // git dir looks like this).
    git_must(&main, &["config", "core.worktree", &main.to_string_lossy()]);
    git_must(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            &lane.to_string_lossy(),
            "-b",
            "lane",
        ],
    );
    let top = |dir: &Path| {
        std::fs::canonicalize(git_must(dir, &["rev-parse", "--show-toplevel"])).expect("top")
    };
    let lane_top = top(&lane);
    let main_top = top(&main);

    write_record(&lane, &a_record(462)).expect("write");
    assert_eq!(read_records(&lane), vec![a_record(462)]);
    assert_eq!(top(&lane), lane_top, "the lane's top level moved");
    assert_eq!(top(&main), main_top, "the main worktree's top level moved");
}

#[test]
#[cfg(unix)]
fn recording_a_claim_keeps_a_relative_core_worktree_working() {
    // A submodule's layout: the git dir lives under the superproject's
    // `.git/modules/<name>` and names its work tree (here at `vendor/sub`,
    // a path other than its name) with a relative path.
    let temp = tempfile::tempdir().expect("tempdir");
    let main = temp.path().join("super").join("vendor").join("sub");
    let gitdir = temp
        .path()
        .join("super")
        .join(".git")
        .join("modules")
        .join("sub");
    let lane = temp.path().join("lane");
    std::fs::create_dir_all(&main).expect("main");
    std::fs::create_dir_all(gitdir.parent().expect("modules")).expect("modules");
    git_must(
        &main,
        &[
            "init",
            "-q",
            "-b",
            "main",
            "--separate-git-dir",
            &gitdir.to_string_lossy(),
        ],
    );
    git_must(&main, &["commit", "-q", "--allow-empty", "-m", "x"]);
    git_must(&main, &["config", "core.worktree", "../../../vendor/sub"]);
    git_must(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            &lane.to_string_lossy(),
            "-b",
            "lane",
        ],
    );
    let top = |dir: &Path| {
        std::fs::canonicalize(git_must(dir, &["rev-parse", "--show-toplevel"])).expect("top")
    };
    let lane_top = top(&lane);
    let main_top = top(&main);

    write_record(&lane, &a_record(462)).expect("write");
    assert_eq!(read_records(&lane), vec![a_record(462)]);
    git_must(&lane, &["status", "--short"]);
    git_must(&main, &["status", "--short"]);
    assert_eq!(top(&lane), lane_top, "the lane's top level moved");
    assert_eq!(top(&main), main_top, "the main worktree's top level moved");
}
