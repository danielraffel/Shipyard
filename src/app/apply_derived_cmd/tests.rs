use std::cell::{Cell, RefCell};
use std::path::Path;
use std::process::Command;

use super::*;
use crate::apply_derived::test_zip::{TestEntry, build};

const REPO: &str = "acme/widgets";
const CONFIG: &str = r#"
[derived_files]
paths = ["test/ctest_script_inputs/", ".shipyard/changed-surface-families.toml"]
drift_tests = ["script-test-inputs-drift"]
carry_trailers = ["Version-Bump"]

[[derived_files.artifacts]]
name = "script-inputs-expected"
root = "test/ctest_script_inputs"

[[derived_files.artifacts]]
name = "families-expected"
root = ""
"#;

struct FakeGitHub {
    heads: Vec<String>,
    reads: Cell<usize>,
    config: Option<String>,
    artifact_name: String,
    /// Override the artifact's `workflow_run.head_sha` (default: the head).
    artifact_head_override: bool,
    /// The overriding head, when overridden; `None` reports no head.
    artifact_head: Option<String>,
    /// The pull request run 200 belongs to.
    run_pr: u64,
    base_ref: String,
    zip: Vec<u8>,
    downloads: Cell<usize>,
    head_repo: String,
    calls: RefCell<Vec<String>>,
}

impl FakeGitHub {
    fn new(head: &str, zip: Vec<u8>) -> Self {
        Self {
            heads: vec![head.to_owned()],
            reads: Cell::new(0),
            config: Some(CONFIG.to_owned()),
            artifact_name: "script-inputs-expected".to_owned(),
            artifact_head_override: false,
            artifact_head: None,
            run_pr: 12,
            base_ref: "main".to_owned(),
            zip,
            downloads: Cell::new(0),
            head_repo: REPO.to_owned(),
            calls: RefCell::new(Vec::new()),
        }
    }

    fn run(&self, id: u64, conclusion: &str, pr: u64) -> RunAtHead {
        RunAtHead {
            id,
            event: "pull_request".to_owned(),
            conclusion: Some(conclusion.to_owned()),
            head_sha: self.heads[0].clone(),
            head_branch: "feature".to_owned(),
            head_repo: REPO.to_owned(),
            pull_requests: vec![pr],
        }
    }
}

impl DerivedGitHub for FakeGitHub {
    fn pull_request(&self, _repo: &str, _number: u64) -> Result<PrHead, String> {
        let at = self.reads.get();
        self.reads.set(at + 1);
        let head = self.heads[at.min(self.heads.len() - 1)].clone();
        Ok(PrHead {
            state: "open".to_owned(),
            head_sha: head,
            head_ref: "feature".to_owned(),
            head_repo: self.head_repo.clone(),
            base_ref: self.base_ref.clone(),
        })
    }

    fn default_branch(&self, _repo: &str) -> Result<String, String> {
        Ok("main".to_owned())
    }

    fn base_config(&self, _repo: &str, base: &str) -> Result<Option<String>, String> {
        self.calls.borrow_mut().push(format!("config {base}"));
        Ok(self.config.clone())
    }

    fn runs_at_head(&self, _repo: &str, head: &str) -> Result<Vec<RunAtHead>, String> {
        self.calls.borrow_mut().push(format!("runs {head}"));
        // 300 is newer than 200 and carries the artifact, but it passed.
        Ok(vec![
            self.run(100, "success", 12),
            self.run(300, "success", 12),
            self.run(200, "failure", self.run_pr),
            self.run(150, "failure", 12),
        ])
    }

    fn run_artifacts(&self, _repo: &str, run_id: u64) -> Result<Vec<ArtifactRef>, String> {
        self.calls.borrow_mut().push(format!("artifacts {run_id}"));
        if run_id != 200 && run_id != 300 {
            return Ok(Vec::new());
        }
        let head_sha = if self.artifact_head_override {
            self.artifact_head.clone()
        } else {
            Some(self.heads[0].clone())
        };
        Ok(vec![
            ArtifactRef {
                id: 9,
                name: "unrelated-logs".to_owned(),
                size: 10,
                expired: false,
                head_sha: None,
            },
            ArtifactRef {
                id: if run_id == 200 { 7 } else { 8 },
                name: self.artifact_name.clone(),
                size: self.zip.len() as u64,
                expired: false,
                head_sha,
            },
        ])
    }

    fn download_artifact(
        &self,
        _repo: &str,
        artifact_id: u64,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        assert_eq!(
            artifact_id, 7,
            "only the failed run's configured artifact is downloaded"
        );
        self.downloads.set(self.downloads.get() + 1);
        Ok(self.zip.clone())
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A checkout of `feature` with one stale fragment, whose `origin` says
/// github.com/acme/widgets but is rewritten to a local bare repository.
struct Fixture {
    _root: tempfile::TempDir,
    work: PathBuf,
    bare: PathBuf,
    head: String,
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let bare = root.path().join("bare.git");
    let work = root.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    git(
        root.path(),
        &["init", "--quiet", "--bare", bare.to_str().unwrap()],
    );
    git(&work, &["init", "--quiet", "-b", "feature"]);
    for (key, value) in [
        ("user.name", "Test"),
        ("user.email", "test@example.com"),
        ("commit.gpgsign", "false"),
        ("remote.origin.url", "git@github.com:acme/widgets.git"),
    ] {
        git(&work, &["config", key, value]);
    }
    git(
        &work,
        &[
            "config",
            &format!("url.{}.insteadOf", bare.display()),
            "git@github.com:acme/widgets.git",
        ],
    );
    let fragments = work.join("test/ctest_script_inputs/tests");
    std::fs::create_dir_all(&fragments).unwrap();
    std::fs::write(fragments.join("a.json"), "{\"sources\": [\"old\"]}\n").unwrap();
    std::fs::write(work.join("README.md"), "readme\n").unwrap();
    git(&work, &["add", "-A"]);
    git(
        &work,
        &[
            "commit",
            "--quiet",
            "-m",
            "feat: change a script",
            "-m",
            "Version-Bump: skip reason=\"test\"\nCo-Authored-By: X <x@example.com>",
        ],
    );
    git(
        &work,
        &["push", "--quiet", "origin", "HEAD:refs/heads/feature"],
    );
    let head = git(&work, &["rev-parse", "HEAD"]);
    Fixture {
        _root: root,
        work,
        bare,
        head,
    }
}

fn good_zip() -> Vec<u8> {
    build(&[
        TestEntry::file("tests/a.json", b"{\"sources\": [\"new\"]}\n"),
        TestEntry::file("executables/b.json", b"{\"exe\": 1}\n"),
    ])
}

fn checkout(fixture: &Fixture) -> Git {
    Git::new(&fixture.work)
        .map_err(|f| f.message().to_owned())
        .unwrap()
}

fn status(fixture: &Fixture) -> String {
    git(
        &fixture.work,
        &["status", "--porcelain", "--untracked-files=all"],
    )
}

#[test]
fn a_good_artifact_applies_and_pushes() {
    let fixture = fixture();
    let gh = FakeGitHub::new(&fixture.head, good_zip());
    let report = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap();
    assert_eq!(report.outcome, Outcome::Pushed);
    assert_eq!(
        report.run_id, 200,
        "the newest failed run with the artifact"
    );
    assert_eq!(report.artifacts, ["script-inputs-expected"]);
    assert_eq!(
        std::fs::read_to_string(fixture.work.join("test/ctest_script_inputs/tests/a.json"))
            .unwrap(),
        "{\"sources\": [\"new\"]}\n"
    );
    assert!(status(&fixture).is_empty(), "everything committed");
    let commit = report.commit.unwrap();
    let message = git(&fixture.work, &["log", "-1", "--format=%B"]);
    assert_eq!(
        message,
        "chore: apply CI's expected derived files from run 200\n\nVersion-Bump: skip \
         reason=\"test\""
    );
    let files = git(&fixture.work, &["show", "--name-only", "--format=", "HEAD"]);
    assert_eq!(
        files,
        "test/ctest_script_inputs/executables/b.json\ntest/ctest_script_inputs/tests/a.json"
    );
    let pushed = git(&fixture.bare, &["rev-parse", "refs/heads/feature"]);
    assert_eq!(pushed, commit);
    assert_eq!(
        git(&fixture.work, &["rev-parse", "HEAD~1"]),
        fixture.head,
        "a fast-forward on the PR head"
    );
}

#[test]
fn dry_run_writes_nothing() {
    let fixture = fixture();
    let gh = FakeGitHub::new(&fixture.head, good_zip());
    let report = run(&gh, &checkout(&fixture), REPO, 12, false).unwrap();
    assert_eq!(report.outcome, Outcome::DryRun);
    let changes: Vec<(&str, Change)> = report
        .files
        .iter()
        .map(|file| (file.path.as_str(), file.change))
        .collect();
    assert_eq!(
        changes,
        [
            ("test/ctest_script_inputs/executables/b.json", Change::New),
            ("test/ctest_script_inputs/tests/a.json", Change::Modified),
        ]
    );
    assert_eq!((report.files[1].added, report.files[1].removed), (1, 1));
    assert!(status(&fixture).is_empty(), "nothing written");
    assert!(
        !fixture
            .work
            .join("test/ctest_script_inputs/executables")
            .exists()
    );
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), fixture.head);
    assert_eq!(
        git(&fixture.bare, &["rev-parse", "refs/heads/feature"]),
        fixture.head
    );
}

#[test]
fn a_path_outside_the_allowlist_is_refused() {
    let fixture = fixture();
    let mut gh = FakeGitHub::new(
        &fixture.head,
        build(&[
            TestEntry::file(".shipyard/changed-surface-families.toml", b"ok = 1\n"),
            TestEntry::file("docs/evil.md", b"pwned\n"),
        ]),
    );
    gh.artifact_name = "families-expected".to_owned();
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(
        error.contains("outside the derived-file allowlist"),
        "{error}"
    );
    assert!(status(&fixture).is_empty());
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), fixture.head);
}

#[test]
fn a_symlink_entry_is_refused() {
    let fixture = fixture();
    let gh = FakeGitHub::new(
        &fixture.head,
        build(&[TestEntry::file("tests/a.json", b"../../../.ssh/id_ed25519").with_mode(0o120_777)]),
    );
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("not a regular file"), "{error}");
    assert!(status(&fixture).is_empty());
}

#[test]
fn a_dotdot_entry_is_refused() {
    let fixture = fixture();
    let gh = FakeGitHub::new(
        &fixture.head,
        build(&[TestEntry::file("../../README.md", b"pwned\n")]),
    );
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("`..` path segment"), "{error}");
    assert_eq!(
        std::fs::read_to_string(fixture.work.join("README.md")).unwrap(),
        "readme\n"
    );
    assert!(status(&fixture).is_empty());
}

#[test]
fn a_moved_head_is_refused() {
    let fixture = fixture();
    let mut gh = FakeGitHub::new(&fixture.head, good_zip());
    gh.heads.push("f".repeat(40));
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("head moved"), "{error}");
    assert!(status(&fixture).is_empty(), "nothing written");
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), fixture.head);
}

#[test]
fn a_checkout_not_at_the_head_is_refused() {
    let fixture = fixture();
    let gh = FakeGitHub::new(&"e".repeat(40), good_zip());
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(
        error.contains("check out the pull request's head"),
        "{error}"
    );
    assert_eq!(gh.downloads.get(), 0);
}

#[test]
fn a_dirty_tree_is_refused() {
    let fixture = fixture();
    std::fs::write(fixture.work.join("scratch.txt"), "local\n").unwrap();
    let gh = FakeGitHub::new(&fixture.head, good_zip());
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("not clean"), "{error}");
    assert_eq!(gh.downloads.get(), 0);
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), fixture.head);
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_in_the_checkout_is_refused() {
    let fixture = fixture();
    let outside = fixture.work.parent().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(
        &outside,
        fixture.work.join("test/ctest_script_inputs/executables"),
    )
    .unwrap();
    git(&fixture.work, &["add", "-A"]);
    git(&fixture.work, &["commit", "--quiet", "-m", "link"]);
    let head = git(&fixture.work, &["rev-parse", "HEAD"]);
    let gh = FakeGitHub::new(&head, good_zip());
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("symlink"), "{error}");
    assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn a_fork_or_unconfigured_base_is_refused() {
    let fixture = fixture();
    let mut gh = FakeGitHub::new(&fixture.head, good_zip());
    gh.head_repo = "mallory/widgets".to_owned();
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("fork"), "{error}");

    let mut gh = FakeGitHub::new(&fixture.head, good_zip());
    gh.config = Some("[project]\nname = \"widgets\"\n".to_owned());
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("no [derived_files]"), "{error}");
    assert_eq!(gh.downloads.get(), 0);
}

#[test]
fn reads_config_from_the_base_never_the_checkout() {
    let fixture = fixture();
    let gh = FakeGitHub::new(&fixture.head, good_zip());
    run(&gh, &checkout(&fixture), REPO, 12, false).unwrap();
    assert!(gh.calls.borrow().contains(&"config main".to_owned()));
}

#[test]
fn parses_remote_urls() {
    for url in [
        "git@github.com:acme/widgets.git",
        "ssh://git@github.com/acme/widgets",
        "https://github.com/acme/widgets.git\n",
    ] {
        assert_eq!(repo_from_url(url).as_deref(), Some(REPO), "{url}");
    }
    assert_eq!(repo_from_url("https://example.com/acme/widgets"), None);
    assert_eq!(repo_from_url("/tmp/bare.git"), None);
}

#[test]
fn parses_artifact_listings() {
    let value = serde_json::json!({"artifacts": [
        {"id": 1, "name": "script-inputs-expected", "size_in_bytes": 5, "expired": false,
         "workflow_run": {"id": 3, "head_sha": "abc"}},
        {"id": 2, "name": "x"}
    ]});
    let parsed = parse_artifacts(&value);
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].head_sha.as_deref(), Some("abc"));
    assert!(parsed[1].expired, "unknown expiry is treated as expired");
}

#[test]
fn an_artifact_that_already_matches_is_refused_not_reported_fixed() {
    let fixture = fixture();
    let gh = FakeGitHub::new(
        &fixture.head,
        build(&[TestEntry::file(
            "tests/a.json",
            b"{\"sources\": [\"old\"]}\n",
        )]),
    );
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("already match the checkout"), "{error}");
    assert!(
        error.contains("test/ctest_script_inputs/tests/a.json"),
        "{error}"
    );
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), fixture.head);
}

#[test]
fn a_mixed_artifact_is_refused_before_anything_is_written() {
    let fixture = fixture();
    let gh = FakeGitHub::new(
        &fixture.head,
        build(&[
            TestEntry::file("tests/a.json", b"{\"sources\": [\"old\"]}\n"),
            TestEntry::file("executables/b.json", b"{\"exe\": 1}\n"),
        ]),
    );
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("already match"), "{error}");
    assert!(status(&fixture).is_empty(), "nothing written");
    assert!(
        !fixture
            .work
            .join("test/ctest_script_inputs/executables")
            .exists()
    );
}

#[test]
fn another_pull_requests_run_is_never_used() {
    let fixture = fixture();
    let mut gh = FakeGitHub::new(&fixture.head, good_zip());
    gh.run_pr = 99;
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("no failed run of #12"), "{error}");
    assert_eq!(gh.downloads.get(), 0);
}

#[test]
fn an_artifact_without_this_head_is_ignored() {
    for artifact_head in [None, Some("b".repeat(40))] {
        let fixture = fixture();
        let mut gh = FakeGitHub::new(&fixture.head, good_zip());
        gh.artifact_head_override = true;
        gh.artifact_head.clone_from(&artifact_head);
        let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
        assert!(
            error.contains("no failed run"),
            "{artifact_head:?}: {error}"
        );
        assert_eq!(gh.downloads.get(), 0);
    }
}

#[test]
fn a_pull_request_off_the_default_branch_is_refused() {
    let fixture = fixture();
    let mut gh = FakeGitHub::new(&fixture.head, good_zip());
    gh.base_ref = "attacker-base".to_owned();
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("not the default branch"), "{error}");
    assert!(
        gh.calls.borrow().is_empty(),
        "no config read from that base"
    );
}

#[test]
fn a_head_moved_before_the_push_is_refused_and_not_pushed() {
    let fixture = fixture();
    let mut gh = FakeGitHub::new(&fixture.head, good_zip());
    // Reads: the first, after the artifact, then before the push.
    gh.heads = vec![fixture.head.clone(), fixture.head.clone(), "f".repeat(40)];
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("before pushing"), "{error}");
    assert!(error.contains("was not pushed"), "{error}");
    assert_eq!(
        git(&fixture.bare, &["rev-parse", "refs/heads/feature"]),
        fixture.head
    );
}

#[cfg(unix)]
#[test]
fn repository_hooks_never_run() {
    let fixture = fixture();
    let marker = fixture.work.parent().unwrap().join("hook-ran");
    let hooks = fixture.work.join(".git/hooks");
    for hook in [
        "pre-commit",
        "commit-msg",
        "post-commit",
        "pre-push",
        "reference-transaction",
    ] {
        crate::test_support::write_executable_script(
            &hooks.join(hook),
            &format!("#!/bin/sh\necho {hook} >> '{}'\n", marker.display()),
        );
    }
    let gh = FakeGitHub::new(&fixture.head, good_zip());
    let report = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap();
    assert_eq!(report.outcome, Outcome::Pushed);
    assert!(
        !marker.exists(),
        "a hook ran: {}",
        std::fs::read_to_string(&marker).unwrap_or_default()
    );
}

#[test]
fn a_tracked_submodule_path_is_refused() {
    let fixture = fixture();
    // An uninitialized submodule: a gitlink over an empty directory, with an
    // artifact entry inside it.
    git(
        &fixture.work,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!(
                "160000,{},test/ctest_script_inputs/executables",
                fixture.head
            ),
        ],
    );
    git(&fixture.work, &["commit", "--quiet", "-m", "gitlink"]);
    std::fs::create_dir_all(fixture.work.join("test/ctest_script_inputs/executables")).unwrap();
    let head = git(&fixture.work, &["rev-parse", "HEAD"]);
    assert!(status(&fixture).is_empty(), "{}", status(&fixture));
    let gh = FakeGitHub::new(&head, good_zip());
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("symlink or submodule"), "{error}");
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), head);
}

#[test]
fn an_ignored_path_is_refused_before_writing() {
    let fixture = fixture();
    std::fs::write(fixture.work.join(".gitignore"), "executables/\n").unwrap();
    git(&fixture.work, &["add", ".gitignore"]);
    git(&fixture.work, &["commit", "--quiet", "-m", "ignore"]);
    let head = git(&fixture.work, &["rev-parse", "HEAD"]);
    let gh = FakeGitHub::new(&head, good_zip());
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("ignore rules cover"), "{error}");
    assert!(
        !fixture
            .work
            .join("test/ctest_script_inputs/executables")
            .exists()
    );
}

/// On a case-insensitive filesystem `tests/A.json` lands on the tracked
/// `tests/a.json`: the post-write check refuses, and the checkout is
/// restored.
#[test]
fn a_write_that_changes_another_path_is_refused_and_restored() {
    let fixture = fixture();
    if !fixture.work.join("readme.MD").exists() {
        return; // case-sensitive filesystem: the case alias does not exist
    }
    let gh = FakeGitHub::new(
        &fixture.head,
        build(&[
            TestEntry::file("tests/A.json", b"{\"sources\": [\"new\"]}\n"),
            TestEntry::file("executables/b.json", b"{\"exe\": 1}\n"),
        ]),
    );
    let error = run(&gh, &checkout(&fixture), REPO, 12, true).unwrap_err();
    assert!(error.contains("outside the plan"), "{error}");
    assert!(error.contains("restored"), "{error}");
    assert!(status(&fixture).is_empty(), "{}", status(&fixture));
    assert!(
        !fixture
            .work
            .join("test/ctest_script_inputs/executables")
            .exists()
    );
    assert_eq!(git(&fixture.work, &["rev-parse", "HEAD"]), fixture.head);
}
