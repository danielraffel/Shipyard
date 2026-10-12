//! Apply the carrier's `update_branch` class.
//!
//! A carry merges the base into a `BEHIND`, armed, reviewer-approved head
//! through GitHub's merge-only `update-branch` endpoint, then re-arms the new
//! head. The approval names the old head, so the carry may extend it to the
//! new head only when the merge changes nothing the reviewer approved. Two
//! facts prove that, and both are checked in the target repository's own
//! clone after fetching the live refs:
//!
//! * **Merge tree.** The new head's tree equals `git merge-tree` of its base
//!   parent and the approved head: GitHub's merge was automatic.
//! * **Own lines.** The pull request's own changes, measured from its merge
//!   base, are the same before and after. They are compared as the added and
//!   removed lines per file (`-U0`, without `index` lines or hunk headers), so
//!   a base change elsewhere in the same file, which moves line numbers and
//!   blob ids, is not a difference, and an identical change already on the
//!   base, which would silently drop a line from the pull request, is.
//!
//! Both facts are predicted before the `PUT`, so a carry that would change
//! the approved lines is never started, and verified after it against the
//! commit GitHub actually created. A verification failure leaves the pull
//! request carried but not re-armed, with the reason in the mutation audit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use sha2::{Digest, Sha256};

#[cfg(not(test))]
const HEAD_POLL_INTERVAL: Duration = Duration::from_secs(3);
#[cfg(test)]
const HEAD_POLL_INTERVAL: Duration = Duration::from_millis(0);
/// Reads of the pull request's head after the update is accepted.
pub(super) const HEAD_POLL_ATTEMPTS: u32 = 20;

/// The local clone of `repo`: the one passed with `--clone`, else the
/// current directory when its `origin` is that repository.
///
/// # Errors
///
/// When no clone is configured, or its `origin` is another repository.
pub(super) fn resolve_clone(
    clones: &BTreeMap<String, PathBuf>,
    cwd: &Path,
    repo: &str,
) -> Result<PathBuf, String> {
    let path = clones
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(repo))
        .map_or_else(|| cwd.to_path_buf(), |(_, path)| path.clone());
    let origin = git(&path, &["remote", "get-url", "origin"]).map_err(|error| {
        format!(
            "no clone of {repo} (pass --clone {repo}=PATH): {} has no origin ({error})",
            path.display()
        )
    })?;
    if origin_names(&origin, repo) {
        Ok(path)
    } else {
        Err(format!(
            "no clone of {repo} (pass --clone {repo}=PATH): {} tracks {origin}",
            path.display()
        ))
    }
}

fn origin_names(origin: &str, repo: &str) -> bool {
    let origin = origin.trim().trim_end_matches('/');
    let origin = origin.strip_suffix(".git").unwrap_or(origin);
    let origin = origin.to_ascii_lowercase();
    let repo = repo.to_ascii_lowercase();
    origin == repo || origin.ends_with(&format!("/{repo}")) || origin.ends_with(&format!(":{repo}"))
}

fn git(clone: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .current_dir(clone)
        .args(args)
        .output()
        .map_err(|error| format!("git {} failed to start: {error}", args.join(" ")))?;
    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end()
        .to_owned())
}

/// Fetch the live base branch and pull-request head into carrier-private refs
/// and return their shas.
fn fetch_pair(clone: &Path, base: &str, pr: u64) -> Result<(String, String), String> {
    let base_ref = format!("refs/shipyard-carrier/{pr}/base");
    let head_ref = format!("refs/shipyard-carrier/{pr}/head");
    git(
        clone,
        &[
            "fetch",
            "--no-tags",
            "--quiet",
            "origin",
            &format!("+refs/heads/{base}:{base_ref}"),
            &format!("+refs/pull/{pr}/head:{head_ref}"),
        ],
    )?;
    Ok((
        git(clone, &["rev-parse", "--verify", &base_ref])?,
        git(clone, &["rev-parse", "--verify", &head_ref])?,
    ))
}

/// The changes from `from` to `to` as added and removed lines per file, with
/// no `index` lines, hunk headers, or context.
fn own_lines(clone: &Path, from: &str, to: &str) -> Result<String, String> {
    let diff = git(
        clone,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            "--binary",
            "-U0",
            from,
            to,
        ],
    )?;
    Ok(diff
        .lines()
        .filter(|line| !line.starts_with("index ") && !line.starts_with("@@"))
        .collect::<Vec<_>>()
        .join("\n"))
}

fn digest(text: &str) -> String {
    format!("{:x}", Sha256::digest(text.as_bytes()))
}

fn merge_tree(clone: &Path, base: &str, head: &str) -> Result<String, String> {
    let tree = git(clone, &["merge-tree", "--write-tree", base, head])
        .map_err(|error| format!("the base does not merge automatically: {error}"))?;
    tree.lines()
        .next()
        .map(str::trim)
        .filter(|tree| tree.len() == 40 && tree.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .map(str::to_owned)
        .ok_or_else(|| "merge-tree did not produce a tree".to_owned())
}

/// What the carry will produce, read before the `PUT`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CarryPrediction {
    /// Base sha the merge will use.
    pub(super) base_sha: String,
    /// Tree of the merge of `base_sha` and the head.
    pub(super) carry_tree: String,
    /// SHA-256 of the pull request's own lines.
    pub(super) own_lines_sha256: String,
}

/// Predict the carry of `head` onto the live `base_sha`.
///
/// # Errors
///
/// When the fetched refs are not the live ones, the merge is not automatic,
/// or the merge would change the pull request's own lines.
pub(super) fn predict(
    clone: &Path,
    base: &str,
    pr: u64,
    head: &str,
    live_base_sha: &str,
) -> Result<CarryPrediction, String> {
    let (base_sha, fetched_head) = fetch_pair(clone, base, pr)?;
    if !fetched_head.eq_ignore_ascii_case(head) {
        return Err(format!(
            "the fetched pull-request head {fetched_head} is not the approved head {head}"
        ));
    }
    if !base_sha.eq_ignore_ascii_case(live_base_sha) {
        return Err(format!(
            "the fetched {base} {base_sha} is not GitHub's live {base} {live_base_sha}"
        ));
    }
    let carry_tree = merge_tree(clone, &base_sha, head)?;
    let merge_base = git(clone, &["merge-base", &base_sha, head])?;
    let approved = own_lines(clone, &merge_base, head)?;
    let carried = own_lines(clone, &base_sha, &carry_tree)?;
    if approved != carried {
        return Err(format!(
            "the carry would change the pull request's own lines (approved {}, carried {}); \
             the base already holds part of this change",
            digest(&approved),
            digest(&carried)
        ));
    }
    Ok(CarryPrediction {
        base_sha,
        carry_tree,
        own_lines_sha256: digest(&approved),
    })
}

/// Verify the commit GitHub created for the carry.
///
/// # Errors
///
/// When the new head is not a merge of the approved head and a commit of the
/// base, its tree is not the automatic merge of those two, or its own lines
/// differ from the approved ones.
pub(super) fn verify(
    clone: &Path,
    base: &str,
    pr: u64,
    head: &str,
    new_head: &str,
    prediction: &CarryPrediction,
) -> Result<String, String> {
    let (base_sha, fetched) = fetch_pair(clone, base, pr)?;
    if !fetched.eq_ignore_ascii_case(new_head) {
        return Err(format!(
            "the fetched pull-request head {fetched} is not the carried head {new_head}"
        ));
    }
    let parents = git(clone, &["rev-list", "--parents", "-n", "1", new_head])?;
    let parents: Vec<&str> = parents.split_whitespace().skip(1).collect();
    let [first, merge_parent] = parents.as_slice() else {
        return Err(format!(
            "carried head {new_head} has {} parents; a merge-only update has two",
            parents.len()
        ));
    };
    if !first.eq_ignore_ascii_case(head) {
        return Err(format!(
            "carried head {new_head} does not descend from the approved head {head}"
        ));
    }
    git(
        clone,
        &["merge-base", "--is-ancestor", merge_parent, &base_sha],
    )
    .map_err(|_| format!("merge parent {merge_parent} is not on {base}"))?;
    let tree = git(clone, &["rev-parse", &format!("{new_head}^{{tree}}")])?;
    let expected = merge_tree(clone, merge_parent, head)?;
    if tree != expected {
        return Err(format!(
            "carried tree {tree} is not the automatic merge {expected} of {merge_parent} and \
             the approved head"
        ));
    }
    let carried = digest(&own_lines(clone, merge_parent, new_head)?);
    if carried != prediction.own_lines_sha256 {
        return Err(format!(
            "carried own lines {carried} differ from the approved {}",
            prediction.own_lines_sha256
        ));
    }
    Ok(tree)
}

/// Sleep between head reads.
pub(super) fn poll_pause() {
    std::thread::sleep(HEAD_POLL_INTERVAL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_urls_name_their_repository() {
        for origin in [
            "https://github.com/Owner/Repo.git",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo",
            "/tmp/x/owner/repo.git",
        ] {
            assert!(origin_names(origin, "owner/repo"), "{origin}");
        }
        for origin in [
            "https://github.com/owner/repo-fork.git",
            "https://github.com/other/repo.git",
        ] {
            assert!(!origin_names(origin, "owner/repo"), "{origin}");
        }
    }
}
