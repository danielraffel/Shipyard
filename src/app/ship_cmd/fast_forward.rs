//! Automatic head adoption for a pull request that only moved forward.
//!
//! Recorded ship-state pins the head SHA it validated, and a different head is
//! refused as drift unless the caller passes `--adopt-head`. That guard exists
//! for rewritten history: after an amend or a force-push the recorded head is
//! no longer part of the branch, so the operator must say, explicitly, that the
//! new tree is the one to validate.
//!
//! A fast-forward is not that case. When the recorded head is an ancestor of
//! the current head, the branch kept every commit Shipyard saw and only grew
//! (a follow-up commit, or a merge of the base branch). That is the most common
//! drift and the least surprising, and refusing it forces an extra flag on the
//! routine path. So a pure descendant on the same base branch adopts the new
//! head without the flag. Adoption goes through the same path as
//! `--adopt-head`: prior runs and evidence are cleared and the new head is
//! validated from scratch, so nothing validated for the old head is ever
//! credited to the new one.
//!
//! Every uncertain answer declines: an unreadable ancestry check, a recorded
//! head that is not a full hex SHA, a missing object, or a base-branch change
//! all fall through to the ordinary drift refusal.

use std::path::Path;
use std::process::{Command, Stdio};

use crate::ship::ShipExecutionRequest;
use crate::ship_state::ShipStateStore;

/// Recorded head that `request` fast-forwards from, when adoption is safe.
///
/// Returns `None` when the request already adopts explicitly, when no state is
/// recorded, when there is no SHA drift, when the base branch changed, or when
/// the recorded head is not provably an ancestor of the requested head.
pub(super) fn fast_forward_adoption(
    cwd: &Path,
    store: &ShipStateStore,
    request: &ShipExecutionRequest,
) -> Option<String> {
    fast_forward_adoption_with(store, request, |ancestor, descendant| {
        git_is_ancestor(cwd, ancestor, descendant)
    })
}

fn fast_forward_adoption_with(
    store: &ShipStateStore,
    request: &ShipExecutionRequest,
    is_ancestor: impl Fn(&str, &str) -> bool,
) -> Option<String> {
    if request.adopt_head {
        return None;
    }
    let existing = store.get_scoped(&request.repo, request.pr)?;
    if !existing.is_sha_drift(&request.sha) || existing.base_branch != request.base_branch {
        return None;
    }
    if !is_full_sha(&existing.head_sha) || !is_full_sha(&request.sha) {
        return None;
    }
    is_ancestor(&existing.head_sha, &request.sha).then_some(existing.head_sha)
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn git_is_ancestor(cwd: &Path, ancestor: &str, descendant: &str) -> bool {
    Command::new("git")
        .current_dir(cwd)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;
    use std::process::Command;

    use super::{fast_forward_adoption, fast_forward_adoption_with};
    use crate::job::{Priority, ValidationMode};
    use crate::ship::ShipExecutionRequest;
    use crate::ship_state::{ShipState, ShipStateStore};

    const OLD: &str = "1111111111111111111111111111111111111111";
    const NEW: &str = "2222222222222222222222222222222222222222";

    fn request(sha: &str, base: &str) -> ShipExecutionRequest {
        ShipExecutionRequest {
            pr: 629,
            repo: "danielraffel/Shipyard".to_owned(),
            branch: "feature/x".to_owned(),
            base_branch: base.to_owned(),
            sha: sha.to_owned(),
            commit_subject: String::new(),
            pr_url: None,
            pr_title: None,
            mode: ValidationMode::Full,
            priority: Priority::Normal,
            warm_disabled: false,
            fail_fast: false,
            resume_from: None,
            advisory_targets: BTreeSet::new(),
            adopt_head: false,
            pr_snapshot_file: None,
            metadata_authority_receipt: None,
            targets: Vec::new(),
        }
    }

    fn store_with_head(head: &str) -> (tempfile::TempDir, ShipStateStore) {
        let temp = tempfile::tempdir().expect("tempdir");
        let store = ShipStateStore::new(temp.path().join("ship")).expect("store");
        store
            .save(&ShipState::new(
                629,
                "danielraffel/Shipyard",
                "feature/x",
                "main",
                head,
                "policy",
            ))
            .expect("save");
        (temp, store)
    }

    #[test]
    fn a_descendant_head_on_the_same_base_is_adopted() {
        let (_temp, store) = store_with_head(OLD);
        let adopted =
            fast_forward_adoption_with(&store, &request(NEW, "main"), |a, d| a == OLD && d == NEW);
        assert_eq!(adopted.as_deref(), Some(OLD));
    }

    #[test]
    fn a_rewritten_head_is_not_adopted() {
        let (_temp, store) = store_with_head(OLD);
        assert_eq!(
            fast_forward_adoption_with(&store, &request(NEW, "main"), |_, _| false),
            None
        );
    }

    #[test]
    fn a_base_branch_change_is_not_adopted_even_when_descendant() {
        let (_temp, store) = store_with_head(OLD);
        assert_eq!(
            fast_forward_adoption_with(&store, &request(NEW, "release"), |_, _| true),
            None
        );
    }

    #[test]
    fn no_drift_and_explicit_adoption_are_left_alone() {
        let (_temp, store) = store_with_head(OLD);
        assert_eq!(
            fast_forward_adoption_with(&store, &request(OLD, "main"), |_, _| true),
            None
        );
        let mut explicit = request(NEW, "main");
        explicit.adopt_head = true;
        assert_eq!(
            fast_forward_adoption_with(&store, &explicit, |_, _| true),
            None
        );
    }

    #[test]
    fn a_non_sha_recorded_head_is_never_handed_to_git() {
        let (_temp, store) = store_with_head("--output=/tmp/x");
        assert_eq!(
            fast_forward_adoption_with(&store, &request(NEW, "main"), |_, _| true),
            None
        );
    }

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(dir)
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
            ])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git");
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout)
            .expect("utf8")
            .trim()
            .to_owned()
    }

    #[test]
    fn real_git_adopts_a_merge_commit_descendant_and_refuses_a_sibling() {
        let repo = tempfile::tempdir().expect("repo");
        let dir = repo.path();
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "base"]);
        git(dir, &["checkout", "-q", "-b", "feature/x"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "pr work"]);
        let old = git(dir, &["rev-parse", "HEAD"]);
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["commit", "-q", "--allow-empty", "-m", "main moved"]);
        git(dir, &["checkout", "-q", "feature/x"]);
        // The 2026-09-28 shape: a normal merge of the base into the PR branch.
        git(dir, &["merge", "-q", "--no-edit", "--no-ff", "main"]);
        let merged = git(dir, &["rev-parse", "HEAD"]);
        git(dir, &["reset", "-q", "--hard", &old]);
        git(
            dir,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "--amend",
                "-m",
                "rewritten",
            ],
        );
        let rewritten = git(dir, &["rev-parse", "HEAD"]);

        let (_temp, store) = store_with_head(&old);
        assert_eq!(
            fast_forward_adoption(dir, &store, &request(&merged, "main")).as_deref(),
            Some(old.as_str())
        );
        assert_eq!(
            fast_forward_adoption(dir, &store, &request(&rewritten, "main")),
            None
        );
    }
}
