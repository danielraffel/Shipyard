//! Pull request title/body composition.
//!
//! The title and body describe the whole branch, not its tip. A branch of
//! several subject-only commits used to open with an empty body and the last
//! commit's subject as its title, and a branch whose tip merged the base in
//! opened as "Merge remote-tracking branch …". Over 47 pull requests one
//! repository's agents opened this way, 32 had their body rewritten by hand
//! afterwards, 30 of them to append the same attribution line.
//!
//! So both are built from the non-merge commits in `origin/<base>..HEAD`,
//! oldest first, skipping mechanical commits (version bumps, changelog
//! regeneration). The body is never empty, and `pr.body.attribution` appends a
//! configured closing line.

use std::path::Path;
use std::process::Command;

use crate::lane_policy::LanePolicy;

const MAX_COMMIT_WALK: usize = 20;
const MECHANICAL_PREFIXES: [&str; 5] = [
    "chore: bump versions",
    "chore(plugin): bump",
    "chore(release):",
    "chore: regenerate changelog",
    "docs: regenerate changelog",
];

/// Config key naming a line appended to every composed pull request body.
pub const ATTRIBUTION_CONFIG_KEY: &str = "pr.body.attribution";

/// Compose a PR title from the branch's own commits.
///
/// The first meaningful commit names the branch, unless a later one carries a
/// conventional type that ranks higher for release notes (`feat` over `fix`
/// over anything else): a feature branch whose first commit is a test is
/// still a feature.
#[must_use]
pub fn compose_pr_title(cwd: &Path, branch: &str, base: &str) -> String {
    let commits = branch_commits(cwd, base);
    let best_rank = commits
        .iter()
        .map(|commit| type_rank(&commit.subject))
        .max();
    commits
        .iter()
        .find(|commit| Some(type_rank(&commit.subject)) == best_rank)
        .map(|commit| commit.subject.clone())
        .or_else(|| meaningful_commit(cwd).map(|commit| commit.subject))
        .filter(|subject| !subject.is_empty())
        .unwrap_or_else(|| title_from_branch(branch))
}

/// Compose a PR body from the branch's own commits.
#[must_use]
pub fn compose_pr_body(cwd: &Path, branch: &str, base: &str, attribution: Option<&str>) -> String {
    compose_pr_body_with_policy(cwd, branch, base, None, attribution)
}

/// Compose a PR body from the branch's commits plus optional advisory-lane
/// policy and a closing attribution line. Never empty.
#[must_use]
pub fn compose_pr_body_with_policy(
    cwd: &Path,
    branch: &str,
    base: &str,
    policy: Option<&LanePolicy>,
    attribution: Option<&str>,
) -> String {
    let mut commits = branch_commits(cwd, base);
    if commits.is_empty()
        && let Some(tip) = meaningful_commit(cwd)
    {
        commits.push(tip);
    }
    let mut sections: Vec<String> = Vec::new();
    match commits.as_slice() {
        [] => sections.push(format!("Changes on `{branch}`.")),
        [only] => sections.push(
            only.body
                .clone()
                .filter(|body| !body.is_empty())
                .unwrap_or_else(|| only.subject.clone()),
        ),
        many => {
            for commit in many {
                let mut section = format!("### {}", commit.subject);
                if let Some(body) = commit.body.as_deref().filter(|body| !body.is_empty()) {
                    section.push_str("\n\n");
                    section.push_str(body);
                }
                sections.push(section);
            }
        }
    }
    if let Some(policy) = policy
        && !policy.advisory_targets.is_empty()
    {
        let mut lines = vec![
            "## Advisory lanes".to_owned(),
            "The following lanes are **advisory** — their status is informational and does not block merge:"
                .to_owned(),
        ];
        lines.extend(policy.advisory_targets.iter().map(|target| {
            let suffix = if policy.overrides_from_trailer.contains(target) {
                " (overridden via Lane-Policy trailer)"
            } else {
                ""
            };
            format!("- `{target}`{suffix}")
        }));
        sections.push(lines.join("\n"));
    }
    let mut body = sections.join("\n\n");
    if let Some(line) = attribution.map(str::trim).filter(|line| !line.is_empty())
        && !body.contains(line)
    {
        body.push_str("\n\n");
        body.push_str(line);
    }
    body
}

/// Marker that opens the provenance block a stamping hook appends.
const PROVENANCE_MARKER: &str = "<!-- whence ";

/// `text`, or the contents of the file named by a leading `@`.
pub fn resolve_body_append(argument: &str, cwd: &Path) -> Result<String, String> {
    let Some(path) = argument.strip_prefix('@') else {
        return Ok(argument.to_owned());
    };
    let path = cwd.join(path);
    std::fs::read_to_string(&path)
        .map_err(|error| format!("--body-append {}: {error}", path.display()))
}

/// `body` with `text` added once: after everything already written (the
/// attribution line included) and before the provenance block, which stays
/// last. `None` when the text is blank or already present, so repeating an
/// append changes nothing.
#[must_use]
pub fn append_to_body(body: &str, text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || body.contains(text) {
        return None;
    }
    let (head, tail) = body
        .find(PROVENANCE_MARKER)
        .map_or((body, ""), |at| body.split_at(at));
    let head = head.trim_end();
    let mut next = if head.is_empty() {
        text.to_owned()
    } else {
        format!("{head}\n\n{text}")
    };
    if !tail.is_empty() {
        next.push_str("\n\n");
        next.push_str(tail);
    }
    Some(next)
}

/// What `apply_body_append` did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BodyAppend {
    /// The body was rewritten with the text added.
    Appended,
    /// The text was already there (or blank); nothing was written.
    AlreadyPresent,
}

/// Read pull request `number`'s body and write it back with `text` appended.
pub fn apply_body_append(
    gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    number: u64,
    text: &str,
) -> Result<BodyAppend, String> {
    let path = format!("repos/{repo}/pulls/{number}");
    let raw = gh(&["api".to_owned(), path.clone()])?;
    let value: serde_json::Value =
        serde_json::from_str(&raw).map_err(|error| format!("unparseable pull request: {error}"))?;
    let body = value
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let Some(next) = append_to_body(body, text) else {
        return Ok(BodyAppend::AlreadyPresent);
    };
    gh(&[
        "api".to_owned(),
        "-X".to_owned(),
        "PATCH".to_owned(),
        path,
        "-f".to_owned(),
        format!("body={next}"),
    ])?;
    Ok(BodyAppend::Appended)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CommitText {
    subject: String,
    body: Option<String>,
}

/// Ranking of a conventional-commit type for naming a branch.
fn type_rank(subject: &str) -> u8 {
    let kind = subject
        .split(':')
        .next()
        .unwrap_or("")
        .split('(')
        .next()
        .unwrap_or("")
        .trim_end_matches('!')
        .to_ascii_lowercase();
    match kind.as_str() {
        "feat" => 2,
        "fix" => 1,
        _ => 0,
    }
}

/// Meaningful non-merge commits in `origin/<base>..HEAD`, oldest first.
fn branch_commits(cwd: &Path, base: &str) -> Vec<CommitText> {
    let Ok(output) = Command::new("git")
        .args([
            "log",
            "--reverse",
            "--no-merges",
            "--format=%s%x1f%b%x1e",
            &format!("origin/{base}..HEAD"),
        ])
        .current_dir(cwd)
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .split('\u{1e}')
        .filter_map(|record| {
            let (subject, body) = record.trim_start_matches('\n').split_once('\u{1f}')?;
            let subject = subject.trim().to_owned();
            (!subject.is_empty() && !is_mechanical_subject(&subject)).then(|| CommitText {
                subject,
                body: Some(body.trim().to_owned()).filter(|body| !body.is_empty()),
            })
        })
        .collect()
}

fn meaningful_commit(cwd: &Path) -> Option<CommitText> {
    (0..MAX_COMMIT_WALK).find_map(|offset| {
        let rev = if offset == 0 {
            "HEAD".to_owned()
        } else {
            format!("HEAD~{offset}")
        };
        let subject = git_log_field(cwd, &rev, "%s")?;
        (!is_mechanical_subject(&subject) && !subject.starts_with("Merge ")).then(|| CommitText {
            subject,
            body: git_log_field(cwd, &rev, "%b"),
        })
    })
}

fn git_log_field(cwd: &Path, rev: &str, format: &str) -> Option<String> {
    let output = Command::new("git")
        .args(["log", "-1", &format!("--format={format}"), rev])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn is_mechanical_subject(subject: &str) -> bool {
    let lowered = subject.to_lowercase();
    MECHANICAL_PREFIXES
        .iter()
        .any(|prefix| lowered.starts_with(prefix))
}

fn title_from_branch(branch: &str) -> String {
    let segment = branch.rsplit('/').next().unwrap_or(branch);
    let mut title = segment.replace(['-', '_'], " ");
    if let Some(first) = title.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    title
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::{Command, Stdio};

    use crate::lane_policy::LanePolicy;

    use super::{
        BodyAppend, append_to_body, apply_body_append, compose_pr_body,
        compose_pr_body_with_policy, compose_pr_title, resolve_body_append,
    };

    const ATTRIBUTION: &str = "🤖 Generated with [Claude Code](https://claude.com/claude-code)";

    fn git(args: &[&str], cwd: &Path) {
        let status = Command::new("git")
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

    fn commit(cwd: &Path, file: &str, message: &[&str]) {
        std::fs::write(cwd.join(file), file).expect("write");
        git(&["add", "."], cwd);
        let mut args = vec!["commit", "-q"];
        for part in message {
            args.push("-m");
            args.push(part);
        }
        git(&args, cwd);
    }

    /// A repository whose `main` is also `origin/main`, with `feature`
    /// checked out on top of it.
    fn seed_repo() -> tempfile::TempDir {
        let temp = tempfile::tempdir().expect("tempdir");
        git(&["init", "--quiet", "--initial-branch=main"], temp.path());
        commit(temp.path(), "README.md", &["seed"]);
        git(
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
            temp.path(),
        );
        git(&["checkout", "-q", "-b", "feature"], temp.path());
        temp
    }

    #[test]
    fn a_branch_of_subject_only_commits_gets_every_commit_in_its_body() {
        let temp = seed_repo();
        commit(temp.path(), "a", &["cli: lock the delegate's builds"]);
        commit(temp.path(), "b", &["gates: run the source-selftest lane"]);
        commit(
            temp.path(),
            "c",
            &["test(ci): keep the harness in its tempdir"],
        );

        assert_eq!(
            compose_pr_title(temp.path(), "feature", "main"),
            "cli: lock the delegate's builds",
            "the branch is named by its first commit, not its last"
        );
        assert_eq!(
            compose_pr_body(temp.path(), "feature", "main", None),
            "### cli: lock the delegate's builds\n\n### gates: run the source-selftest lane\n\n### test(ci): keep the harness in its tempdir"
        );
    }

    #[test]
    fn a_merge_of_the_base_at_the_tip_names_nothing() {
        let temp = seed_repo();
        commit(temp.path(), "a", &["fix: repair the gate", "Why it broke."]);
        git(&["checkout", "-q", "main"], temp.path());
        commit(temp.path(), "m", &["main moved"]);
        git(
            &["update-ref", "refs/remotes/origin/main", "HEAD"],
            temp.path(),
        );
        git(&["checkout", "-q", "feature"], temp.path());
        git(
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "Merge remote-tracking branch 'origin/main' into feature",
                "origin/main",
            ],
            temp.path(),
        );

        assert_eq!(
            compose_pr_title(temp.path(), "feature", "main"),
            "fix: repair the gate"
        );
        assert_eq!(
            compose_pr_body(temp.path(), "feature", "main", None),
            "Why it broke."
        );
    }

    #[test]
    fn a_feature_is_titled_as_a_feature_even_when_a_test_came_first() {
        let temp = seed_repo();
        commit(temp.path(), "a", &["test(queue): prove the argv parity"]);
        commit(temp.path(), "b", &["fix(queue): tolerate a missing head"]);
        commit(
            temp.path(),
            "c",
            &["feat(queue): certify an un-implicated head"],
        );
        commit(temp.path(), "d", &["feat(queue): a second feature"]);

        assert_eq!(
            compose_pr_title(temp.path(), "feature", "main"),
            "feat(queue): certify an un-implicated head"
        );
    }

    #[test]
    fn mechanical_commits_name_nothing_and_a_lone_commit_uses_its_body() {
        let temp = seed_repo();
        commit(temp.path(), "a", &["Add feature", "The reason."]);
        commit(temp.path(), "VERSION", &["chore: bump versions"]);

        assert_eq!(
            compose_pr_title(temp.path(), "feature", "main"),
            "Add feature"
        );
        assert_eq!(
            compose_pr_body(temp.path(), "feature", "main", None),
            "The reason."
        );

        let bare = seed_repo();
        commit(bare.path(), "a", &["Add feature"]);
        assert_eq!(
            compose_pr_body(bare.path(), "feature", "main", None),
            "Add feature",
            "a subject-only commit still yields a body"
        );
    }

    #[test]
    fn the_attribution_line_closes_the_body_once() {
        let temp = seed_repo();
        commit(temp.path(), "a", &["Add feature", "The reason."]);
        assert_eq!(
            compose_pr_body(temp.path(), "feature", "main", Some(ATTRIBUTION)),
            format!("The reason.\n\n{ATTRIBUTION}")
        );
        assert_eq!(
            compose_pr_body(temp.path(), "feature", "main", Some("  ")),
            "The reason.",
            "a blank setting adds nothing"
        );

        let stamped = seed_repo();
        commit(
            stamped.path(),
            "a",
            &["Add feature", &format!("The reason.\n\n{ATTRIBUTION}")],
        );
        assert_eq!(
            compose_pr_body(stamped.path(), "feature", "main", Some(ATTRIBUTION))
                .matches(ATTRIBUTION)
                .count(),
            1
        );
    }

    #[test]
    fn without_git_the_title_comes_from_the_branch_and_the_body_is_not_empty() {
        let temp = tempfile::tempdir().expect("tempdir");

        assert_eq!(
            compose_pr_title(temp.path(), "feature/fix-shipyard_pin", "main"),
            "Fix shipyard pin"
        );
        assert_eq!(
            compose_pr_body(temp.path(), "feature/fix-shipyard_pin", "main", None),
            "Changes on `feature/fix-shipyard_pin`."
        );
    }

    #[test]
    fn body_appends_advisory_lanes_before_the_attribution() {
        let temp = seed_repo();
        commit(temp.path(), "a", &["Add feature", "Why"]);
        let policy = LanePolicy {
            advisory_targets: ["windows".to_owned()].into_iter().collect(),
            overrides_from_trailer: ["windows".to_owned()].into_iter().collect(),
        };

        assert_eq!(
            compose_pr_body_with_policy(
                temp.path(),
                "feature",
                "main",
                Some(&policy),
                Some(ATTRIBUTION)
            ),
            format!(
                "Why\n\n## Advisory lanes\nThe following lanes are **advisory** — their status is informational and does not block merge:\n- `windows` (overridden via Lane-Policy trailer)\n\n{ATTRIBUTION}"
            )
        );
    }

    const STAMP: &str = "<!-- whence {\"prov\": {\"session\": \"s1\"}} -->\n\n---\n### Provenance\n<!-- /whence -->";

    #[test]
    fn appended_text_lands_after_the_attribution_and_before_the_provenance_block() {
        let body = format!("Why.\n\n{ATTRIBUTION}\n\n{STAMP}");
        let next = append_to_body(&body, "Proxy: 3 of 9 before.\n").expect("appended");
        assert_eq!(
            next,
            format!("Why.\n\n{ATTRIBUTION}\n\nProxy: 3 of 9 before.\n\n{STAMP}")
        );
        assert_eq!(
            append_to_body(&next, "Proxy: 3 of 9 before."),
            None,
            "idempotent"
        );
        assert_eq!(append_to_body(&body, "  \n"), None, "blank adds nothing");
    }

    #[test]
    fn without_a_provenance_block_the_text_closes_the_body() {
        assert_eq!(
            append_to_body("Why.\n", "Note.").as_deref(),
            Some("Why.\n\nNote.")
        );
        assert_eq!(append_to_body("", "Note.").as_deref(), Some("Note."));
        assert_eq!(
            append_to_body(STAMP, "Note.").as_deref(),
            Some(format!("Note.\n\n{STAMP}").as_str())
        );
    }

    #[test]
    fn apply_reads_the_live_body_and_writes_only_when_something_changes() {
        use std::cell::RefCell;

        let body = RefCell::new(format!("Why.\n\n{STAMP}"));
        let writes = RefCell::new(0);
        let gh = |args: &[String]| -> Result<String, String> {
            if args.iter().any(|arg| arg == "PATCH") {
                assert_eq!(args[3], "repos/o/r/pulls/7");
                let next = args
                    .iter()
                    .find_map(|arg| arg.strip_prefix("body="))
                    .expect("body field");
                *body.borrow_mut() = next.to_owned();
                *writes.borrow_mut() += 1;
                return Ok("{}".to_owned());
            }
            Ok(serde_json::json!({ "body": *body.borrow() }).to_string())
        };
        assert_eq!(
            apply_body_append(&gh, "o/r", 7, "Note."),
            Ok(BodyAppend::Appended)
        );
        assert!(body.borrow().contains("Note.\n\n<!-- whence"));
        assert_eq!(
            apply_body_append(&gh, "o/r", 7, "Note."),
            Ok(BodyAppend::AlreadyPresent)
        );
        assert_eq!(*writes.borrow(), 1, "the repeat wrote nothing");

        let null_body = |args: &[String]| -> Result<String, String> {
            if args.iter().any(|arg| arg == "PATCH") {
                assert!(args.iter().any(|arg| arg == "body=Note."));
                return Ok("{}".to_owned());
            }
            Ok(r#"{"body": null}"#.to_owned())
        };
        assert_eq!(
            apply_body_append(&null_body, "o/r", 8, "Note."),
            Ok(BodyAppend::Appended)
        );
    }

    #[test]
    fn an_at_argument_reads_a_file_relative_to_the_checkout() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("note.md"), "From a file.\n").expect("write");
        assert_eq!(
            resolve_body_append("@note.md", temp.path()).as_deref(),
            Ok("From a file.\n")
        );
        assert_eq!(
            resolve_body_append("plain", temp.path()).as_deref(),
            Ok("plain")
        );
        assert!(resolve_body_append("@missing.md", temp.path()).is_err());
    }
}
