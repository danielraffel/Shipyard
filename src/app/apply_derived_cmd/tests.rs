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
            zip,
            downloads: Cell::new(0),
            head_repo: REPO.to_owned(),
            calls: RefCell::new(Vec::new()),
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
            base_ref: "main".to_owned(),
        })
    }

    fn base_config(&self, _repo: &str, base: &str) -> Result<Option<String>, String> {
        self.calls.borrow_mut().push(format!("config {base}"));
        Ok(self.config.clone())
    }

    fn runs_at_head(&self, _repo: &str, head: &str) -> Result<Vec<RunAtHead>, String> {
        self.calls.borrow_mut().push(format!("runs {head}"));
        Ok(vec![
            RunAtHead {
                id: 100,
                conclusion: Some("success".to_owned()),
            },
            RunAtHead {
                id: 200,
                conclusion: Some("failure".to_owned()),
            },
            RunAtHead {
                id: 150,
                conclusion: Some("failure".to_owned()),
            },
        ])
    }

    fn run_artifacts(&self, _repo: &str, run_id: u64) -> Result<Vec<ArtifactRef>, String> {
        self.calls.borrow_mut().push(format!("artifacts {run_id}"));
        if run_id != 200 {
            return Ok(Vec::new());
        }
        Ok(vec![
            ArtifactRef {
                id: 9,
                name: "unrelated-logs".to_owned(),
                size: 10,
                expired: false,
                head_sha: None,
            },
            ArtifactRef {
                id: 7,
                name: self.artifact_name.clone(),
                size: self.zip.len() as u64,
                expired: false,
                head_sha: Some(self.heads[0].clone()),
            },
        ])
    }

    fn download_artifact(
        &self,
        _repo: &str,
        artifact_id: u64,
        _max_bytes: usize,
    ) -> Result<Vec<u8>, String> {
        assert_eq!(artifact_id, 7, "only the configured artifact is downloaded");
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
            TestEntry::file(".github/workflows/evil.yml", b"on: push\n"),
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
