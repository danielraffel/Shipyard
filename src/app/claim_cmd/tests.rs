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
