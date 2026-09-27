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

    use super::{compose_pr_body, compose_pr_body_with_policy, compose_pr_title};

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
}
