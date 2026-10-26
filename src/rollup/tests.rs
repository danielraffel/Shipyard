//! A real git repository stands in for GitHub: compares, parents, trees and
//! modes come from git itself, so every rollup below is built the way
//! `shipyard rollup build` builds one.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};

use super::*;
use crate::head_approval::ApprovalPolicy;

const REPO: &str = "o/r";
const DATE: &str = "2026-10-01T00:00:00Z";

struct Git {
    dir: tempfile::TempDir,
}

impl Git {
    fn new() -> Self {
        let git = Self {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        git.run(&["init", "-q", "-b", "main"]);
        let base = git.commit(
            None,
            &[
                ("a.txt", Some("a\n")),
                ("b.txt", Some("b\n")),
                ("c.txt", Some("c\n")),
                ("d.txt", Some("d\n")),
                ("shared.txt", Some("one\ntwo\nthree\nfour\nfive\nsix\n")),
            ],
        );
        git.run(&["update-ref", "refs/heads/main", &base]);
        git
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn try_run_env(&self, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(self.path())
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .env("GIT_AUTHOR_DATE", DATE)
            .env("GIT_COMMITTER_DATE", DATE)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", self.path());
        for (key, value) in env {
            command.env(key, value);
        }
        let output = command.output().map_err(|error| error.to_string())?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(format!(
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ))
        }
    }

    fn run(&self, args: &[&str]) -> String {
        self.try_run_env(args, &[])
            .unwrap_or_else(|error| panic!("git {args:?}: {error}"))
            .trim()
            .to_owned()
    }

    fn sha(&self, rev: &str) -> String {
        self.run(&["rev-parse", rev])
    }

    /// A commit on `parent` (or a root) with `files` written (`None`
    /// removes), plus `modes` set to executable.
    fn commit_with(
        &self,
        parents: &[&str],
        tree_of: Option<&str>,
        files: &[(&str, Option<&str>)],
        executable: &[&str],
    ) -> String {
        let index = self.path().join(".git").join("rollup-test-index");
        let index = index.to_str().expect("utf-8 path");
        let env = [("GIT_INDEX_FILE", index)];
        let _ = std::fs::remove_file(index);
        if let Some(tree) = tree_of {
            self.try_run_env(&["read-tree", tree], &env)
                .expect("read-tree");
        }
        for (name, content) in files {
            match content {
                Some(content) => {
                    let blob_path = self.path().join(".git").join("rollup-test-blob");
                    std::fs::write(&blob_path, content).expect("write blob");
                    let blob = self.run(&["hash-object", "-w", blob_path.to_str().expect("path")]);
                    self.try_run_env(
                        &[
                            "update-index",
                            "--add",
                            "--cacheinfo",
                            &format!("100644,{blob},{name}"),
                        ],
                        &env,
                    )
                    .expect("update-index");
                }
                None => {
                    self.try_run_env(&["update-index", "--force-remove", name], &env)
                        .expect("remove");
                }
            }
        }
        for name in executable {
            let staged = self
                .try_run_env(&["ls-files", "-s", name], &env)
                .expect("ls-files");
            let blob = staged.split(' ').nth(1).expect("staged blob").to_owned();
            self.try_run_env(
                &[
                    "update-index",
                    "--cacheinfo",
                    &format!("100755,{blob},{name}"),
                ],
                &env,
            )
            .expect("chmod");
        }
        let tree = self
            .try_run_env(&["write-tree"], &env)
            .expect("write-tree")
            .trim()
            .to_owned();
        let mut args = vec![
            "commit-tree".to_owned(),
            tree,
            "-m".to_owned(),
            "c".to_owned(),
        ];
        for parent in parents {
            args.push("-p".to_owned());
            args.push((*parent).to_owned());
        }
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args)
    }

    fn commit(&self, parent: Option<&str>, files: &[(&str, Option<&str>)]) -> String {
        let parents: Vec<&str> = parent.into_iter().collect();
        self.commit_with(&parents, parent, files, &[])
    }

    fn advance_main(&self, files: &[(&str, Option<&str>)]) -> String {
        let main = self.sha("main");
        let next = self.commit(Some(&main), files);
        self.run(&["update-ref", "refs/heads/main", &next]);
        next
    }

    fn build(&self, base: &str, members: &[(u64, String)]) -> Vec<String> {
        build_commits(&|args: &[&str]| self.try_run_env(args, &[]), base, members)
            .expect("rollup builds")
    }

    fn compare(&self, range: &str) -> Value {
        let (from, to) = range.split_once("...").expect("three-dot range");
        let (from, to) = (self.sha(from), self.sha(to));
        let base = self.run(&["merge-base", &from, &to]);
        let raw = self.run(&["diff", "--raw", "--no-abbrev", "-M", &base, &to]);
        let files: Vec<Value> = raw
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (meta, paths) = line.split_once('\t').expect("raw line");
                let meta: Vec<&str> = meta.split(' ').collect();
                let paths: Vec<&str> = paths.split('\t').collect();
                let (old, new, letter) = (meta[2], meta[3], &meta[4][..1]);
                match letter {
                    "A" => json!({"filename": paths[0], "status": "added", "sha": new, "patch": true}),
                    "D" => json!({"filename": paths[0], "status": "removed", "sha": old, "patch": true}),
                    "R" => json!({"filename": paths[1], "status": "renamed", "sha": new,
                                  "previous_filename": paths[0], "patch": true}),
                    _ => json!({"filename": paths[0], "status": "modified", "sha": new, "patch": true}),
                }
            })
            .collect();
        json!({"merge_base": base, "files": files})
    }

    fn tree_entries(&self, expression: &str) -> Value {
        let (commit, directory) = expression.split_once(':').expect("expression");
        let spec = if directory.is_empty() {
            commit.to_owned()
        } else {
            format!("{commit}:{directory}")
        };
        let listing = self.run(&["ls-tree", &spec]);
        let entries: Vec<Value> = listing
            .lines()
            .map(|line| {
                let (meta, name) = line.split_once('\t').expect("ls-tree line");
                let mode = meta.split(' ').next().expect("mode");
                json!({"name": name, "mode": u64::from_str_radix(mode, 8).expect("octal")})
            })
            .collect();
        json!({"entries": entries})
    }
}

#[derive(Clone)]
struct Pull {
    number: u64,
    branch: String,
    head: String,
    head_repo: String,
    state: &'static str,
    merged: bool,
    draft: bool,
    merged_at: Option<String>,
}

struct Hub<'a> {
    git: &'a Git,
    pulls: RefCell<Vec<Pull>>,
    config: Option<String>,
    allow_merge_commit: bool,
    rules: Value,
    required: Value,
    checks: Value,
    /// Moved onto by the merge call itself, before it merges: the base moving
    /// between the re-read and the merge.
    race: RefCell<Option<String>>,
    comments: BTreeMap<u64, Vec<Value>>,
    writes: RefCell<Vec<Vec<String>>>,
    /// Run once just before the base tip is read, to move it under a reader.
    before_tip: RefCell<Option<Box<dyn FnOnce() + 'a>>>,
}

const ROLLUP_CONFIG: &str = "[train]\nenabled = true\nmode = \"rollup\"\n";

impl<'a> Hub<'a> {
    fn new(git: &'a Git) -> Self {
        Self {
            git,
            pulls: RefCell::new(Vec::new()),
            config: Some(ROLLUP_CONFIG.to_owned()),
            allow_merge_commit: true,
            rules: json!([]),
            required: json!([{"context": "build", "integration_id": null}]),
            checks: json!([{"id": 1, "name": "build", "status": "completed",
                            "conclusion": "success", "app": 15368}]),
            race: RefCell::new(None),
            comments: BTreeMap::new(),
            writes: RefCell::new(Vec::new()),
            before_tip: RefCell::new(None),
        }
    }

    fn pull(&self, number: u64, branch: &str, head: &str) {
        self.pulls.borrow_mut().push(Pull {
            number,
            branch: branch.to_owned(),
            head: head.to_owned(),
            head_repo: REPO.to_owned(),
            state: "open",
            merged: false,
            draft: false,
            merged_at: None,
        });
    }

    fn find(&self, number: u64) -> Pull {
        self.pulls
            .borrow()
            .iter()
            .find(|pull| pull.number == number)
            .cloned()
            .unwrap_or_else(|| panic!("no PR #{number}"))
    }

    fn run(&self, args: &[String]) -> Result<String, String> {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let jq = args
            .iter()
            .position(|arg| *arg == "--jq")
            .map_or("", |index| args[index + 1]);
        if args.contains(&"graphql") {
            let query = args
                .iter()
                .find(|arg| arg.starts_with("query="))
                .expect("query");
            let mut repository = serde_json::Map::new();
            for (index, piece) in query.split("object(expression:").skip(1).enumerate() {
                let expression: String = serde_json::Deserializer::from_str(piece)
                    .into_iter::<String>()
                    .next()
                    .expect("expression")
                    .expect("string");
                repository.insert(format!("d{index}"), self.git.tree_entries(&expression));
            }
            return Ok(json!({"data": {"repository": repository}}).to_string());
        }
        match args.as_slice() {
            ["api", "-H", _, path] if path.contains("/contents/") => self
                .config
                .clone()
                .ok_or_else(|| "HTTP 404: Not Found".to_owned()),
            ["api", "-X", method, path, ..] => {
                self.writes
                    .borrow_mut()
                    .push(args.iter().map(|arg| (*arg).to_owned()).collect());
                if *method == "PUT" && path.ends_with("/merge") {
                    let head = args
                        .iter()
                        .find_map(|arg| arg.strip_prefix("sha="))
                        .expect("sha bound");
                    if let Some(moved) = self.race.borrow_mut().take() {
                        self.git.run(&["update-ref", "refs/heads/main", &moved]);
                    }
                    let main = self.git.sha("main");
                    let merge = self.git.commit_with(&[&main, head], Some(head), &[], &[]);
                    self.git.run(&["update-ref", "refs/heads/main", &merge]);
                    return Ok(json!({"merged": true, "sha": merge}).to_string());
                }
                Ok("{}".to_owned())
            }
            ["api", "--paginate", path, ..] => {
                let number: u64 = path
                    .split('/')
                    .nth(4)
                    .and_then(|part| part.parse().ok())
                    .expect("number");
                if path.ends_with("/comments") {
                    Ok(self
                        .comments
                        .get(&number)
                        .into_iter()
                        .flatten()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join("\n"))
                } else {
                    Ok(String::new())
                }
            }
            ["api", path, ..] => self.read(path.strip_prefix("repos/o/r").expect("repo"), jq),
            other => panic!("unexpected gh call {other:?}"),
        }
    }

    fn read(&self, path: &str, jq: &str) -> Result<String, String> {
        let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        match parts.as_slice() {
            [""] => Ok(self.allow_merge_commit.to_string()),
            ["pulls", number] => {
                let pull = self.find(number.parse().expect("number"));
                if jq.contains("join") {
                    return Ok(format!("{}\nmain", pull.branch));
                }
                if jq.starts_with("{state, merged}") {
                    return Ok(json!({"state": pull.state, "merged": pull.merged}).to_string());
                }
                Ok(json!({"ref": pull.branch, "sha": pull.head, "base": "main",
                          "state": pull.state, "head_repo": pull.head_repo})
                .to_string())
            }
            ["compare", range] if jq == ".ahead_by" => {
                let (from, to) = range.split_once("...").expect("range");
                Ok(self
                    .git
                    .run(&["rev-list", "--count", &format!("{from}..{to}")]))
            }
            ["compare", range] => Ok(self.git.compare(range).to_string()),
            [list] if list.starts_with("pulls?state=closed") => Ok(json!(
                self.pulls
                    .borrow()
                    .iter()
                    .filter(|pull| pull.state == "closed")
                    .map(|pull| json!({"number": pull.number, "ref": pull.branch,
                                    "sha": pull.head, "merged_at": pull.merged_at}))
                    .collect::<Vec<_>>()
            )
            .to_string()),
            ["git", "ref", "heads", "main"] => {
                if let Some(hook) = self.before_tip.borrow_mut().take() {
                    hook();
                }
                Ok(self.git.sha("main"))
            }
            ["commits", sha] if jq.contains("parents") => {
                let line = self.git.run(&["rev-list", "--parents", "-n", "1", sha]);
                let parents: Vec<&str> = line.split(' ').skip(1).collect();
                Ok(json!(parents).to_string())
            }
            ["commits", sha] if jq.contains("message") => {
                Ok(self.git.run(&["log", "-1", "--format=%B", sha]))
            }
            ["commits", _] => Ok(DATE.to_owned()),
            ["commits", sha, "pulls"] => Ok(json!(
                self.pulls
                    .borrow()
                    .iter()
                    .filter(|pull| pull.head == *sha)
                    .map(|pull| json!({"number": pull.number, "state": pull.state,
                                    "draft": pull.draft, "sha": pull.head, "base": "main"}))
                    .collect::<Vec<_>>()
            )
            .to_string()),
            ["commits", _, check] if check.starts_with("check-runs") => Ok(self.checks.to_string()),
            ["rules", "branches", "main"] if jq.contains("required_status_checks") => {
                Ok(json!(self.required).to_string())
            }
            ["rules", "branches", "main"] => Ok(self.rules.to_string()),
            ["branches", "main", "protection", "required_status_checks"] => {
                Err("HTTP 404: Not Found".to_owned())
            }
            other => panic!("unexpected read {other:?}"),
        }
    }
}

// The member gate's own signature, so it passes where one is expected.
#[allow(clippy::unnecessary_wraps)]
fn approved(_: u64, _: &str) -> Result<HeadGate, String> {
    Ok(HeadGate::Approved {
        approval: Approval::Review {
            login: "reviewer".to_owned(),
            performed_via_github_app_id: None,
        },
    })
}

fn config() -> Config {
    Config {
        enabled: true,
        mode: Mode::Rollup,
        ..Config::default()
    }
}

/// Members #11 (a.txt) and #12 (b.txt) on the base, rolled up as PR #20.
struct Fixture {
    git: Git,
    base: String,
    heads: Vec<String>,
    merges: Vec<String>,
}

impl Fixture {
    fn new(files: &[&str]) -> Self {
        let git = Git::new();
        let base = git.sha("main");
        let heads: Vec<String> = files
            .iter()
            .map(|name| git.commit(Some(&base), &[(name, Some("changed\n"))]))
            .collect();
        let members: Vec<(u64, String)> = (11..).zip(heads.iter().cloned()).collect();
        let merges = git.build(&base, &members);
        Self {
            git,
            base,
            heads,
            merges,
        }
    }

    fn head(&self) -> &str {
        self.merges.last().expect("a merge")
    }

    fn hub(&self) -> Hub<'_> {
        let hub = Hub::new(&self.git);
        for (pr, head) in (11..).zip(&self.heads) {
            hub.pull(pr, &format!("feature-{pr}"), head);
        }
        hub.pull(20, "shipyard-train/rollup-11", self.head());
        hub
    }
}

fn accept_at(hub: &Hub<'_>, head: &str, config: &Config) -> Result<Rollup, String> {
    let run = |args: &[String]| hub.run(args);
    accept(&run, REPO, 20, head, config, &[], &approved)
}

#[test]
fn the_train_table_is_off_by_default_and_refuses_bad_values() {
    let off = Config::from_text(None).expect("no file");
    assert!(!off.rolls_up());
    assert_eq!(off.max_members, MEMBER_CAP);
    assert!(off.exclude_paths.is_empty() && off.hot_files.is_empty());
    assert!(off.require_counted_approval);
    assert_eq!(off.landing, Landing::MergeQueue);
    let on = Config::from_text(Some(
        "[train]\nenabled = true\nmode = \"rollup\"\nlanding = \"direct\"\nmax_members = 2\n\
         exclude_paths = [\"vendor/**\"]\nhot_files = [\"**/CMakeLists.txt\"]\n",
    ))
    .expect("valid");
    assert!(on.rolls_up());
    assert_eq!(on.landing, Landing::Direct);
    assert_eq!(on.max_members, 2);
    // Enabled with another mode is not a rollup base.
    assert!(
        !Config::from_text(Some("[train]\nenabled = true\nmode = \"burst\"\n"))
            .expect("valid")
            .rolls_up()
    );
    for bad in [
        "[train]\nmode = \"fast\"\n",
        "[train]\nmax_members = 5\n",
        "[train]\nmax_members = 0\n",
        "[train]\nlanding = \"push\"\n",
        "[train]\nexclude_paths = \"x\"\n",
        "[train]\nenabled = \"yes\"\n",
        "train = 1\n",
    ] {
        assert!(Config::from_text(Some(bad)).is_err(), "{bad}");
    }
}

#[test]
fn the_kill_switch_and_pause_file_stop_the_train() {
    for value in ["off", "0", "false", "OFF", "off:o/r", "OFF:O/R"] {
        assert!(stopped(REPO, Some(value), None).is_some(), "{value}");
    }
    // The merge train's switch scopes only `off:`.
    for value in ["on", "", "off:other/repo", "false:o/r"] {
        assert!(stopped(REPO, Some(value), None).is_none(), "{value}");
    }
    let dir = tempfile::tempdir().expect("tempdir");
    assert!(stopped(REPO, None, Some(dir.path())).is_none());
    std::fs::write(dir.path().join(PAUSE_FILE), "paused\n").expect("pause");
    assert!(stopped(REPO, None, Some(dir.path())).is_some());
}

#[test]
fn a_rollup_of_exact_approved_heads_is_accepted() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let rollup = accept_at(&hub, fixture.head(), &config()).expect("accepted");
    assert_eq!(rollup.base_sha, fixture.base);
    assert_eq!(
        rollup
            .members
            .iter()
            .map(|member| (member.pr, member.head.clone(), member.merge.clone()))
            .collect::<Vec<_>>(),
        vec![
            (11, fixture.heads[0].clone(), fixture.merges[0].clone()),
            (12, fixture.heads[1].clone(), fixture.merges[1].clone()),
        ]
    );
    assert_eq!(rollup.members[0].files, vec!["a.txt".to_owned()]);
}

#[test]
fn each_member_reverts_alone_with_revert_m_1() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let git = &fixture.git;
    git.run(&["checkout", "-q", "--detach", fixture.head()]);
    git.run(&["revert", "-m", "1", "--no-edit", &fixture.merges[0]]);
    // Reverting the first member leaves exactly the second member on the base.
    let only_second = git.build(&fixture.base, &[(12, fixture.heads[1].clone())]);
    assert_eq!(
        git.sha("HEAD^{tree}"),
        git.sha(&format!("{}^{{tree}}", only_second[0]))
    );
}

#[test]
fn a_member_that_conflicts_does_not_build() {
    let git = Git::new();
    let base = git.sha("main");
    let first = git.commit(Some(&base), &[("a.txt", Some("one\n"))]);
    let second = git.commit(Some(&base), &[("a.txt", Some("two\n"))]);
    let built = build_commits(
        &|args: &[&str]| git.try_run_env(args, &[]),
        &base,
        &[(1, first), (2, second)],
    );
    assert!(built.expect_err("conflict").contains("member #2"));
}

#[test]
fn a_rollup_is_refused_when_the_base_does_not_take_them_or_the_host_stops_it() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let error = accept_at(&hub, fixture.head(), &Config::default()).expect_err("off");
    assert!(error.contains("does not take rollups"), "{error}");
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join(PAUSE_FILE), "").expect("pause");
    let error =
        head_approval::with_guards_dir(dir.path(), || accept_at(&hub, fixture.head(), &config()))
            .expect_err("paused");
    assert!(error.contains("paused"), "{error}");
}

#[test]
fn a_member_whose_approval_is_carried_held_or_missing_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let run = |args: &[String]| hub.run(args);
    let carried = |pr: u64, head: &str| {
        if pr == 12 {
            Ok(HeadGate::Approved {
                approval: Approval::Carried {
                    from_head: "0".repeat(40),
                    approval: Box::new(Approval::Review {
                        login: "reviewer".to_owned(),
                        performed_via_github_app_id: None,
                    }),
                    rebase: false,
                },
            })
        } else {
            approved(pr, head)
        }
    };
    let error =
        accept(&run, REPO, 20, fixture.head(), &config(), &[], &carried).expect_err("carried");
    assert!(error.contains("carried"), "{error}");
    // With require_counted_approval off, a carried approval counts.
    let lenient = Config {
        require_counted_approval: false,
        ..config()
    };
    assert!(accept(&run, REPO, 20, fixture.head(), &lenient, &[], &carried).is_ok());
    let held = |pr: u64, head: &str| {
        if pr == 11 {
            Ok(HeadGate::NotApproved {
                head: head.to_owned(),
            })
        } else {
            approved(pr, head)
        }
    };
    let error = accept(&run, REPO, 20, fixture.head(), &config(), &[], &held).expect_err("held");
    assert!(error.contains("member #11"), "{error}");
    let own = |pr: u64, head: &str| {
        Ok(HeadGate::SelfApproved {
            head: head.to_owned(),
            agent: format!("agent-{pr}"),
        })
    };
    assert!(accept(&run, REPO, 20, fixture.head(), &config(), &[], &own).is_err());
}

#[test]
fn a_member_head_that_is_no_longer_its_pull_requests_head_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[1].head = "e".repeat(40);
    let error = accept_at(&hub, fixture.head(), &config()).expect_err("moved");
    assert!(error.contains("not the current head"), "{error}");
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[0].state = "closed";
    assert!(accept_at(&hub, fixture.head(), &config()).is_err());
}

#[test]
fn an_added_commit_or_an_edit_inside_a_merge_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let git = &fixture.git;
    // A plain commit on top of the last merge.
    let added = git.commit(Some(fixture.head()), &[("c.txt", Some("sneaked\n"))]);
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].head.clone_from(&added);
    let error = accept_at(&hub, &added, &config()).expect_err("added commit");
    assert!(error.contains("not a two-parent merge"), "{error}");
    // A merge of the right parents whose tree also edits another file.
    let edited = git.commit_with(
        &[&fixture.merges[0], &fixture.heads[1]],
        Some(fixture.head()),
        &[("c.txt", Some("sneaked\n"))],
        &[],
    );
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].head.clone_from(&edited);
    let error = accept_at(&hub, &edited, &config()).expect_err("edited merge");
    assert!(error.contains("c.txt"), "{error}");
    // A merge that changes a member's file away from its approved blob.
    let altered = git.commit_with(
        &[&fixture.merges[0], &fixture.heads[1]],
        Some(fixture.head()),
        &[("b.txt", Some("not what was approved\n"))],
        &[],
    );
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].head.clone_from(&altered);
    let error = accept_at(&hub, &altered, &config()).expect_err("altered blob");
    assert!(error.contains("b.txt"), "{error}");
}

#[test]
fn a_mode_change_inside_a_merge_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let chmod = fixture.git.commit_with(
        &[&fixture.merges[0], &fixture.heads[1]],
        Some(fixture.head()),
        &[],
        &["b.txt"],
    );
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].head.clone_from(&chmod);
    let error = accept_at(&hub, &chmod, &config()).expect_err("mode");
    assert!(error.contains("mode"), "{error}");
}

#[test]
fn a_merge_of_something_other_than_a_member_head_is_refused() {
    let fixture = Fixture::new(&["a.txt"]);
    let git = &fixture.git;
    let stray = git.commit(Some(&fixture.base), &[("b.txt", Some("stray\n"))]);
    let merged = git.build(
        &fixture.base,
        &[(11, fixture.heads[0].clone()), (99, stray)],
    );
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[1].head.clone_from(&merged[1]);
    let error = accept_at(&hub, &merged[1], &config()).expect_err("stray");
    assert!(error.contains("not the current head"), "{error}");
}

#[test]
fn members_that_share_a_file_are_refused() {
    let git = Git::new();
    let base = git.sha("main");
    let first = git.commit(
        Some(&base),
        &[("shared.txt", Some("ONE\ntwo\nthree\nfour\nfive\nsix\n"))],
    );
    let second = git.commit(
        Some(&base),
        &[("shared.txt", Some("one\ntwo\nthree\nfour\nfive\nSIX\n"))],
    );
    // Git merges them cleanly; the rollup still refuses.
    let merges = git.build(&base, &[(11, first.clone()), (12, second.clone())]);
    let hub = Hub::new(&git);
    hub.pull(11, "f1", &first);
    hub.pull(12, "f2", &second);
    hub.pull(20, "shipyard-train/rollup-11", &merges[1]);
    let error = accept_at(&hub, &merges[1], &config()).expect_err("overlap");
    assert!(error.contains("both change shared.txt"), "{error}");
}

#[test]
fn a_base_change_to_a_member_file_since_its_merge_base_is_refused() {
    let git = Git::new();
    let old_base = git.sha("main");
    let member = git.commit(
        Some(&old_base),
        &[("shared.txt", Some("MEMBER\ntwo\nthree\nfour\nfive\nsix\n"))],
    );
    let other = git.commit(Some(&old_base), &[("b.txt", Some("other\n"))]);
    // The base then changes another part of the member's file, and the
    // rollup is formed on that newer base: git merges it cleanly, so the
    // member's file at the rollup head is not the blob anyone approved.
    let base = git.advance_main(&[("shared.txt", Some("one\ntwo\nthree\nfour\nfive\nBASE\n"))]);
    let merges = git.build(&base, &[(11, member.clone()), (12, other.clone())]);
    let hub = Hub::new(&git);
    hub.pull(11, "f1", &member);
    hub.pull(12, "f2", &other);
    hub.pull(20, "shipyard-train/rollup-11", &merges[1]);
    let error = accept_at(&hub, &merges[1], &config()).expect_err("base touched");
    assert!(error.contains("base changed shared.txt"), "{error}");
    // A base change to some other file is fine.
    let git = Git::new();
    let old_base = git.sha("main");
    let member = git.commit(Some(&old_base), &[("a.txt", Some("member\n"))]);
    let base = git.advance_main(&[("d.txt", Some("base\n"))]);
    let merges = git.build(&base, &[(11, member.clone())]);
    let hub = Hub::new(&git);
    hub.pull(11, "f1", &member);
    hub.pull(20, "shipyard-train/rollup-11", &merges[0]);
    accept_at(&hub, &merges[0], &config()).expect("unrelated base change");
}

#[test]
fn a_base_that_moved_onto_a_member_file_after_forming_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    fixture.git.advance_main(&[("d.txt", Some("elsewhere\n"))]);
    let rollup = accept_at(&hub, fixture.head(), &config()).expect("unrelated move");
    assert_ne!(rollup.base_tip, rollup.base_sha);
    fixture.git.advance_main(&[("b.txt", Some("base edit\n"))]);
    let error = accept_at(&hub, fixture.head(), &config()).expect_err("moved onto b.txt");
    assert!(error.contains("form it again"), "{error}");
}

#[test]
fn excluded_and_hot_paths_are_refused() {
    let git = Git::new();
    let base = git.sha("main");
    let workflow = git.commit(
        Some(&base),
        &[(".github/workflows/ci.yml", Some("on: push\n"))],
    );
    let other = git.commit(Some(&base), &[("a.txt", Some("x\n"))]);
    let merges = git.build(&base, &[(11, workflow.clone()), (12, other.clone())]);
    let hub = Hub::new(&git);
    hub.pull(11, "f1", &workflow);
    hub.pull(12, "f2", &other);
    hub.pull(20, "shipyard-train/rollup-11", &merges[1]);
    let run = |args: &[String]| hub.run(args);
    let carry_excluded = ApprovalPolicy::default().carry_excluded_paths();
    let error = accept(
        &run,
        REPO,
        20,
        &merges[1],
        &config(),
        &carry_excluded,
        &approved,
    )
    .expect_err("carry exclusion");
    assert!(error.contains(".github/workflows/ci.yml"), "{error}");
    // The base's own config is excluded too: a member never changes the
    // policy its rollup is judged by.
    let config_change = git.commit(Some(&base), &[(".shipyard/config.toml", Some("[train]\n"))]);
    let config_merges = git.build(&base, &[(13, config_change.clone())]);
    let config_hub = Hub::new(&git);
    config_hub.pull(13, "f3", &config_change);
    config_hub.pull(20, "shipyard-train/rollup-13", &config_merges[0]);
    let config_run = |args: &[String]| config_hub.run(args);
    let error = accept(
        &config_run,
        REPO,
        20,
        &config_merges[0],
        &config(),
        &carry_excluded,
        &approved,
    )
    .expect_err("config exclusion");
    assert!(error.contains(".shipyard/config.toml"), "{error}");
    // The train's own exclusions, empty by default, apply too.
    assert!(accept(&run, REPO, 20, &merges[1], &config(), &[], &approved).is_ok());
    let excluding = Config {
        exclude_paths: vec!["a.txt".to_owned()],
        ..config()
    };
    assert!(accept(&run, REPO, 20, &merges[1], &excluding, &[], &approved).is_err());
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let hot = Config {
        hot_files: vec!["*.txt".to_owned()],
        ..config()
    };
    let error = accept_at(&hub, fixture.head(), &hot).expect_err("hot");
    assert!(error.contains("hot file"), "{error}");
}

#[test]
fn more_members_than_max_members_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt", "c.txt"]);
    let hub = fixture.hub();
    let two = Config {
        max_members: 2,
        ..config()
    };
    let error = accept_at(&hub, fixture.head(), &two).expect_err("too many");
    assert!(error.contains("within 2 merges"), "{error}");
    accept_at(&hub, fixture.head(), &config()).expect("three of four");
}

#[test]
fn only_an_in_repository_rollup_branch_is_judged() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].head_repo = "fork/r".to_owned();
    assert!(accept_at(&hub, fixture.head(), &config()).is_err());
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].branch = "feature-rollup".to_owned();
    let run = |args: &[String]| hub.run(args);
    let policy = ApprovalPolicy::default();
    assert_eq!(
        guard_accept(&run, REPO, 20, fixture.head(), &policy, &approved),
        Ok(None)
    );
    let mut hub = fixture.hub();
    hub.config = None;
    let run = |args: &[String]| hub.run(args);
    assert_eq!(
        guard_accept(&run, REPO, 20, fixture.head(), &policy, &approved),
        Ok(None)
    );
}

fn reviewer_policy() -> ApprovalPolicy {
    ApprovalPolicy {
        required: true,
        reviewer_logins: vec!["shipyard-review[bot]".to_owned()],
        ..ApprovalPolicy::default()
    }
}

fn verdict(verdict: &str, head: &str) -> Value {
    json!({"id": 1, "login": "shipyard-review[bot]", "type": "Bot",
           "at": "2026-10-04T01:00:00Z", "body": format!("verdict: {verdict} head:{head}")})
}

#[test]
fn the_head_approval_gate_approves_a_rollup_of_approved_members_and_respects_a_hold() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let mut hub = fixture.hub();
    hub.comments
        .insert(11, vec![verdict("APPROVE", &fixture.heads[0])]);
    hub.comments
        .insert(12, vec![verdict("APPROVE", &fixture.heads[1])]);
    let evaluate = |hub: &Hub<'_>| {
        let run = |args: &[String]| hub.run(args);
        head_approval::evaluate_with_policy_for_author_with_events(
            &run,
            REPO,
            20,
            fixture.head(),
            &reviewer_policy(),
            &Value::Null,
            None,
        )
        .expect("evaluates")
        .0
    };
    let gate = evaluate(&hub);
    let HeadGate::Approved {
        approval: Approval::Rollup { members },
    } = &gate
    else {
        panic!("not a rollup approval: {gate:?}");
    };
    assert_eq!(
        members.iter().map(|member| member.pr).collect::<Vec<_>>(),
        vec![11, 12]
    );
    assert!(gate.explain().contains("rollup of #11@"));
    // A HOLD on the rollup's own head stops it.
    hub.comments
        .insert(20, vec![verdict("HOLD", fixture.head())]);
    assert!(!evaluate(&hub).allows());
    hub.comments.remove(&20);
    // A member without its own approval stops it.
    hub.comments.remove(&12);
    assert!(!evaluate(&hub).allows());
    // A HOLD after a member's approval stops it.
    hub.comments.insert(
        12,
        vec![
            verdict("APPROVE", &fixture.heads[1]),
            json!({"id": 2, "login": "shipyard-review[bot]", "type": "Bot",
                   "at": "2026-10-04T02:00:00Z",
                   "body": format!("verdict: HOLD head:{}", fixture.heads[1])}),
        ],
    );
    assert!(!evaluate(&hub).allows());
}

#[test]
fn only_the_merge_method_merge_lands_a_rollup() {
    let git = Git::new();
    let mut hub = Hub::new(&git);
    let run = |hub: &Hub<'_>, landing| {
        let run = |args: &[String]| hub.run(args);
        merge_method_refusal(&run, REPO, "main", landing).expect("reads")
    };
    assert!(run(&hub, Landing::Direct).is_none());
    let error = run(&hub, Landing::MergeQueue).expect("no queue");
    assert!(error.contains("no merge queue"), "{error}");
    hub.rules = json!([{"type": "merge_queue", "parameters": {"merge_method": "SQUASH"}}]);
    let error = run(&hub, Landing::MergeQueue).expect("squash queue");
    assert!(error.contains("SQUASH"), "{error}");
    assert!(run(&hub, Landing::Direct).is_some());
    hub.rules = json!([{"type": "merge_queue", "parameters": {"merge_method": "MERGE"}}]);
    assert!(run(&hub, Landing::MergeQueue).is_none());
    hub.rules = json!([{"type": "pull_request",
                         "parameters": {"allowed_merge_methods": ["squash", "rebase"]}}]);
    let error = run(&hub, Landing::Direct).expect("no merge method");
    assert!(error.contains("MERGE"), "{error}");
    hub.rules = json!([]);
    hub.allow_merge_commit = false;
    let error = run(&hub, Landing::Direct).expect("merge commits off");
    assert!(error.contains("does not allow merge commits"), "{error}");
}

#[test]
fn a_direct_landing_refuses_a_moved_base_and_merges_nothing() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let rollup = accept_at(&hub, fixture.head(), &config()).expect("accepted");
    let git = &fixture.git;
    *hub.before_tip.borrow_mut() = Some(Box::new(|| {
        git.advance_main(&[("d.txt", Some("moved\n"))]);
    }));
    let run = |args: &[String]| hub.run(args);
    let mut records = Vec::new();
    let error = land_direct(&run, REPO, &rollup, &mut |record| {
        records.push(record.clone());
        Ok(())
    })
    .expect_err("stale base");
    assert!(error.contains("moved from"), "{error}");
    assert!(hub.writes.borrow().is_empty(), "nothing merged");
    // The tested record was written first, naming what was tested.
    assert_eq!(records[0]["tested_sha"], json!(fixture.head()));
    assert_eq!(records[0]["merged_base"], json!(fixture.base));
}

#[test]
fn a_direct_landing_binds_the_head_and_the_merge_method() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let rollup = accept_at(&hub, fixture.head(), &config()).expect("accepted");
    let run = |args: &[String]| hub.run(args);
    let landed = land_direct(&run, REPO, &rollup, &mut |_| Ok(())).expect("lands");
    assert_eq!(landed.untested, None);
    assert_eq!(landed.merge_commit, fixture.git.sha("main"));
    let writes = hub.writes.borrow();
    assert_eq!(writes.len(), 1, "no alert on a tested landing");
    assert_eq!(
        writes[0],
        vec![
            "api".to_owned(),
            "-X".to_owned(),
            "PUT".to_owned(),
            "repos/o/r/pulls/20/merge".to_owned(),
            "-f".to_owned(),
            format!("sha={}", fixture.head()),
            "-f".to_owned(),
            "merge_method=merge".to_owned(),
        ]
    );
}

#[test]
fn a_base_that_moves_between_the_re_read_and_the_merge_is_an_untested_landing() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let rollup = accept_at(&hub, fixture.head(), &config()).expect("accepted");
    let moved = fixture
        .git
        .commit(Some(&fixture.base), &[("d.txt", Some("raced\n"))]);
    *hub.race.borrow_mut() = Some(moved.clone());
    let run = |args: &[String]| hub.run(args);
    let mut records = Vec::new();
    let landed = land_direct(&run, REPO, &rollup, &mut |record| {
        records.push(record.clone());
        Ok(())
    })
    .expect("it still landed");
    let why = landed.untested.expect("untested");
    assert!(why.contains("never tested"), "{why}");
    // Recorded as untested, after the tested record, naming where it landed.
    assert_eq!(records.len(), 2);
    assert_eq!(records[1]["untested"], json!(true));
    assert_eq!(records[1]["landed_on"][0], json!(moved));
    // And announced on the rollup pull request.
    let writes = hub.writes.borrow();
    assert_eq!(writes[1][3], "repos/o/r/issues/20/comments");
    assert!(writes[1][5].contains("Untested landing"));
}

#[test]
fn required_checks_count_only_the_latest_check_run_from_the_bound_app() {
    let git = Git::new();
    let mut hub = Hub::new(&git);
    let head = git.sha("main");
    let refusal = |hub: &Hub<'_>| {
        let run = |args: &[String]| hub.run(args);
        let required = required_checks(&run, REPO, "main").expect("reads");
        checks_refusal(&run, REPO, &head, &required).expect("reads")
    };
    assert!(refusal(&hub).is_none());
    hub.checks = json!([{"id": 1, "name": "build", "status": "completed",
                         "conclusion": "failure", "app": 15368}]);
    assert!(refusal(&hub).expect("failed").contains("build"));
    hub.checks = json!([{"id": 1, "name": "build", "status": "in_progress",
                         "conclusion": null, "app": 15368}]);
    assert!(refusal(&hub).is_some());
    // A later failed run outweighs an earlier success.
    hub.checks = json!([
        {"id": 1, "name": "build", "status": "completed", "conclusion": "success", "app": 15368},
        {"id": 2, "name": "build", "status": "completed", "conclusion": "failure", "app": 15368},
    ]);
    assert!(refusal(&hub).is_some());
    // Bound to an App: only that App's run counts.
    hub.required = json!([{"context": "build", "integration_id": 42}]);
    hub.checks = json!([{"id": 3, "name": "build", "status": "completed",
                         "conclusion": "success", "app": 7}]);
    let why = refusal(&hub).expect("wrong app");
    assert!(why.contains("from App 42"), "{why}");
    hub.checks = json!([{"id": 3, "name": "build", "status": "completed",
                         "conclusion": "success", "app": 42}]);
    assert!(refusal(&hub).is_none());
    // A commit status is never read as a check run.
    hub.checks = json!([]);
    assert!(
        refusal(&hub)
            .expect("status only")
            .contains("commit status")
    );
    hub.required = json!([]);
    assert!(
        refusal(&hub)
            .expect("none required")
            .contains("requires no checks")
    );
}

fn landed_members_of(fixture: &Fixture) -> Vec<LandedMember> {
    (11..)
        .zip(fixture.heads.iter().zip(&fixture.merges))
        .map(|(pr, (head, merge))| LandedMember {
            pr,
            head: head.clone(),
            merge: merge.clone(),
        })
        .collect()
}

/// Land the fixture's rollup on main the way a merge queue would.
fn land_by_queue(fixture: &Fixture, hub: &Hub<'_>, merged_at: &str) {
    let main = fixture.git.sha("main");
    let merge = fixture
        .git
        .commit_with(&[&main, fixture.head()], Some(fixture.head()), &[], &[]);
    fixture.git.run(&["update-ref", "refs/heads/main", &merge]);
    let mut pulls = hub.pulls.borrow_mut();
    let rollup = pulls
        .iter_mut()
        .find(|pull| pull.number == 20)
        .expect("rollup");
    rollup.state = "closed";
    rollup.merged = true;
    rollup.merged_at = Some(merged_at.to_owned());
}

#[test]
fn members_are_closed_only_once_their_heads_are_proven_on_the_base() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    let members = landed_members_of(&fixture);
    let run = |args: &[String]| hub.run(args);
    // Not landed yet: neither head is on main, so nothing is closed.
    let (closed, left) =
        close_landed_members(&run, REPO, "main", 20, &members, &mut || false).expect("reads");
    assert!(closed.is_empty());
    assert_eq!(left.len(), 2);
    assert!(left[0].1.contains("never closed unproven"), "{}", left[0].1);
    assert!(hub.writes.borrow().is_empty());
    land_by_queue(&fixture, &hub, DATE);
    hub.pulls.borrow_mut()[0].merged = true;
    hub.pulls.borrow_mut()[0].state = "closed";
    let (closed, left) =
        close_landed_members(&run, REPO, "main", 20, &members, &mut || false).expect("reads");
    assert_eq!((closed, left), (vec![12], vec![]));
    let writes = hub.writes.borrow();
    assert_eq!(writes.len(), 2);
    assert!(writes[0][5].contains(&format!("merge commit {}", fixture.merges[1])));
    assert_eq!(writes[1][3], "repos/o/r/pulls/12");
}

#[test]
fn closing_waits_for_github_to_mark_members_merged() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    land_by_queue(&fixture, &hub, DATE);
    let members = landed_members_of(&fixture);
    let run = |args: &[String]| hub.run(args);
    let mut waits = 0;
    let mut settle = || {
        waits += 1;
        // GitHub marks both merged during the second wait.
        if waits == 2 {
            for pull in hub.pulls.borrow_mut().iter_mut().take(2) {
                pull.merged = true;
                pull.state = "closed";
            }
        }
        waits < 5
    };
    let (closed, left) =
        close_landed_members(&run, REPO, "main", 20, &members, &mut settle).expect("reads");
    assert_eq!((closed, left), (vec![], vec![]));
    assert_eq!(waits, 2);
    assert!(hub.writes.borrow().is_empty());
}

#[test]
fn a_queue_landing_is_reconciled_by_the_steward_after_the_settle_time() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    land_by_queue(&fixture, &hub, "2026-10-26T10:00:00Z");
    let run = |args: &[String]| hub.run(args);
    let at = |text: &str| text.parse::<chrono::DateTime<chrono::Utc>>().expect("time");
    // Within the settle time GitHub may still mark them: nothing yet.
    let early = reconcile_if_enabled(&run, REPO, "main", at("2026-10-26T10:01:00Z"));
    assert_eq!(early, Ok(vec![]));
    assert!(hub.writes.borrow().is_empty());
    let lines =
        reconcile_if_enabled(&run, REPO, "main", at("2026-10-26T10:03:00Z")).expect("reconciles");
    assert_eq!(
        lines,
        vec![
            "closed #11: landed via rollup #20".to_owned(),
            "closed #12: landed via rollup #20".to_owned()
        ]
    );
    assert_eq!(hub.writes.borrow().len(), 4);
}

#[test]
fn reconciliation_does_nothing_on_a_base_that_does_not_take_rollups() {
    let fixture = Fixture::new(&["a.txt"]);
    let mut hub = fixture.hub();
    hub.config = Some("[train]\nenabled = false\n".to_owned());
    land_by_queue(&fixture, &hub, DATE);
    let run = |args: &[String]| hub.run(args);
    assert_eq!(
        reconcile_if_enabled(&run, REPO, "main", chrono::Utc::now()),
        Ok(vec![])
    );
    assert!(hub.writes.borrow().is_empty());
}

#[test]
fn a_merge_that_carries_another_members_change_is_refused() {
    // The first merge already brings both members' changes and the second
    // merge changes nothing: the head's tree is exactly right, so only the
    // per-merge replay check sees that merge 1 is not member #11 alone.
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let git = &fixture.git;
    let first = git.commit_with(
        &[&fixture.base, &fixture.heads[0]],
        Some(fixture.head()),
        &[],
        &[],
    );
    let second = git.commit_with(&[&first, &fixture.heads[1]], Some(fixture.head()), &[], &[]);
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[2].head.clone_from(&second);
    let error = accept_at(&hub, &second, &config()).expect_err("replay");
    assert!(error.contains("the rollup's merge"), "{error}");
    assert!(error.contains("b.txt"), "{error}");
}

#[test]
fn a_member_head_shared_by_two_open_pull_requests_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    hub.pull(30, "feature-copy", &fixture.heads[1]);
    let error = accept_at(&hub, fixture.head(), &config()).expect_err("two PRs");
    assert!(error.contains("more than one open pull request"), "{error}");
}

#[test]
fn a_draft_member_is_refused() {
    let fixture = Fixture::new(&["a.txt", "b.txt"]);
    let hub = fixture.hub();
    hub.pulls.borrow_mut()[1].draft = true;
    let error = accept_at(&hub, fixture.head(), &config()).expect_err("draft");
    assert!(error.contains("member #12 is a draft"), "{error}");
}

fn candidate(pr: u64, files: &[&str]) -> Candidate {
    Candidate {
        pr,
        head: format!("{pr:040}"),
        files: files.iter().map(|name| (*name).to_owned()).collect(),
        blocked: None,
    }
}

#[test]
fn formation_takes_the_oldest_ready_disjoint_members() {
    let mut blocked = candidate(3, &["z"]);
    blocked.blocked = Some("not approved".to_owned());
    let candidates = vec![
        candidate(9, &["e"]),
        candidate(5, &["a", "b"]),
        candidate(6, &["b"]),
        blocked,
        candidate(7, &["src/x/CMakeLists.txt"]),
        candidate(8, &["src/y/CMakeLists.txt"]),
        candidate(4, &["tools/ci/run.sh"]),
        candidate(10, &["f"]),
        candidate(11, &["g"]),
    ];
    let hot = Config {
        hot_files: vec!["**/CMakeLists.txt".to_owned()],
        ..config()
    };
    let excluded = ApprovalPolicy::default().carry_excluded_paths();
    let (chosen, skipped) = form(&candidates, &hot, &excluded);
    assert_eq!(chosen, vec![5, 7, 9, 10]);
    let why: BTreeMap<u64, String> = skipped.into_iter().collect();
    assert!(why[&3].contains("not approved"));
    assert!(why[&4].contains("tools/ci/run.sh"));
    assert!(why[&6].contains("#5 also changes b"));
    assert!(why[&8].contains("hot file"));
    assert!(why[&11].contains("full"));
}

#[test]
fn a_failed_rollup_is_bisected_by_halves_down_to_its_culprit() {
    assert_eq!(split(&[1, 2, 3, 4], true), Split::Land);
    assert_eq!(
        split(&[1, 2, 3, 4], false),
        Split::Halves {
            first: vec![1, 2],
            second: vec![3, 4]
        }
    );
    assert_eq!(
        split(&[1, 2, 3], false),
        Split::Halves {
            first: vec![1, 2],
            second: vec![3]
        }
    );
    assert_eq!(split(&[3], false), Split::Eject { pr: 3 });
    assert_eq!(
        after_halves(&[1, 2], true, &[3, 4], true),
        Split::SharedCause
    );
    assert_eq!(
        after_halves(&[1, 2], true, &[3, 4], false),
        Split::Halves {
            first: vec![3],
            second: vec![4]
        }
    );
    assert_eq!(
        after_halves(&[1], false, &[2], true),
        Split::Eject { pr: 1 }
    );
}

#[test]
fn the_notice_names_each_member_for_people_only() {
    let body = notice(&[(11, "a".repeat(40)), (12, "b".repeat(40))]);
    assert!(body.contains(&format!("rollup-of: #11@{}", "a".repeat(40))));
    assert!(!body.contains("reviewer:"));
    assert!(!body.contains("verdict:"));
}
