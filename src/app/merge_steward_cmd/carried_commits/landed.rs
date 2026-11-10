//! A carried commit whose change is already on the base does not hold P.
//!
//! The carry guard names the commits P has that another open pull request Q
//! introduced. A commit whose change the base already has adds nothing Q
//! owns: a merge of Q's branch that only re-applies content which landed
//! under other shas (a rollup of a merged parent), or a cherry-pick of a
//! landed commit. Such a commit is excused here, and only when the steward
//! proves the equivalence itself against the base's tip, read once and then
//! pinned by sha. Nothing is taken from pull request metadata.
//!
//! * A non-merge commit is equivalent when its `git patch-id --stable` equals
//!   that of a single-parent commit reachable from the tip and the candidate's
//!   changed entries (mode, type and blob) are still identical on the pinned
//!   tip. Candidates are the tip's commits that touch the commit's first
//!   changed file since the commit's author date (a cherry-pick or rebase keeps
//!   the author date), at most [`MAX_PATCH_CANDIDATES`] of them.
//! * A merge commit is equivalent when every path it changes relative to its
//!   first parent has, on the tip, an identical entry (mode, type and blob).
//!   The changed paths come from a tree-to-tree walk of the merge against its
//!   first parent, not from a file list, so a truncated list cannot hide a
//!   path. A path the merge removes must be absent on the tip.
//!
//! Anything not provably equivalent keeps the overlap, and so keeps refusing:
//! an unreadable ref, commit, tree, diff or listing, a truncated tree, an
//! orphan commit, an empty diff, or a read over a cap. The excuse is all or
//! nothing: if any shared commit is not equivalent, the overlap is unchanged.

use std::collections::{BTreeMap, BTreeSet};

use super::{Carried, Judgement};
use crate::head_approval::RunGh;

/// Most shared commits examined for one overlap; more keeps the overlap.
const MAX_LANDED_CHECKS: usize = 20;

/// Most commits of the tip fetched and compared by patch-id for one commit.
const MAX_PATCH_CANDIDATES: usize = 20;

/// Most tree reads for one overlap.
const MAX_TREE_READS: usize = 150;

/// GitHub's commit diff endpoint can omit files at this cap.
const MAX_DIFF_FILES: usize = 300;

/// Deepest tree level walked.
const MAX_DEPTH: usize = 40;

/// One entry of a git tree.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Entry {
    mode: String,
    kind: String,
    sha: String,
}

type Tree = BTreeMap<String, Entry>;

/// Drop an overlap whose every shared commit is already on `base`. `None`
/// when all are equivalent; otherwise `settled` unchanged. An approved Q is
/// not examined.
pub(super) fn without_landed_commits(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    settled: (Carried, Judgement),
) -> Option<(Carried, Judgement)> {
    let (Carried::Shares { shared, .. }, judgement) = &settled else {
        return Some(settled);
    };
    if *judgement == Judgement::Approved || shared.is_empty() || shared.len() > MAX_LANDED_CHECKS {
        return Some(settled);
    }
    let mut reader = Reader::new(run_gh, repo);
    let Ok(tip) = reader.tip(base) else {
        return Some(settled);
    };
    for sha in shared {
        if !matches!(reader.landed(sha, &tip), Ok(true)) {
            return Some(settled);
        }
    }
    None
}

/// The base's tip, pinned: its sha and root tree.
struct Tip {
    sha: String,
    tree: String,
}

/// A commit's tree, parents and author date.
struct Commit {
    tree: String,
    parents: Vec<String>,
    date: String,
}

struct Reader<'a> {
    run_gh: RunGh<'a>,
    repo: &'a str,
    trees: BTreeMap<String, Tree>,
    tree_reads: usize,
}

impl<'a> Reader<'a> {
    fn new(run_gh: RunGh<'a>, repo: &'a str) -> Self {
        Self {
            run_gh,
            repo,
            trees: BTreeMap::new(),
            tree_reads: 0,
        }
    }

    fn json(&self, argv: &[String], what: &str) -> Result<serde_json::Value, String> {
        let raw = (self.run_gh)(argv).map_err(|error| format!("{what} unreadable: {error}"))?;
        serde_json::from_str(&raw).map_err(|error| format!("{what} malformed: {error}"))
    }

    fn tip(&self, base: &str) -> Result<Tip, String> {
        let repo = self.repo;
        let reference = self.json(
            &[
                "api".to_owned(),
                format!("repos/{repo}/git/ref/heads/{base}"),
                "--jq".to_owned(),
                "{sha: .object.sha, kind: .object.type}".to_owned(),
            ],
            "base ref",
        )?;
        if reference.get("kind").and_then(|v| v.as_str()) != Some("commit") {
            return Err("the base ref is not a commit".to_owned());
        }
        let sha = text(&reference, "sha").ok_or("the base ref has no sha")?;
        let commit = self.commit(&sha)?;
        Ok(Tip {
            sha,
            tree: commit.tree,
        })
    }

    fn commit(&self, sha: &str) -> Result<Commit, String> {
        let repo = self.repo;
        let value = self.json(
            &[
                "api".to_owned(),
                format!("repos/{repo}/git/commits/{sha}"),
                "--jq".to_owned(),
                "{tree: .tree.sha, parents: [.parents[].sha], date: .author.date}".to_owned(),
            ],
            &format!("commit {sha}"),
        )?;
        let parents = value
            .get("parents")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("commit {sha} has no parent list"))?
            .iter()
            .map(|p| p.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| format!("commit {sha} has a malformed parent"))?;
        Ok(Commit {
            tree: text(&value, "tree").ok_or_else(|| format!("commit {sha} has no tree"))?,
            parents,
            date: text(&value, "date").ok_or_else(|| format!("commit {sha} has no date"))?,
        })
    }

    /// Whether `sha`'s change is already on the tip.
    fn landed(&mut self, sha: &str, tip: &Tip) -> Result<bool, String> {
        let commit = self.commit(sha)?;
        match commit.parents.as_slice() {
            [] => Ok(false),
            [_] => self.patch_on_tip(sha, &commit, tip),
            [first, ..] => {
                let first = self.commit(first)?;
                self.merge_on_tip(&commit.tree, &first.tree, &tip.tree)
            }
        }
    }

    fn diff(&self, sha: &str) -> Result<String, String> {
        let repo = self.repo;
        let diff = (self.run_gh)(&[
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github.diff".to_owned(),
            format!("repos/{repo}/commits/{sha}"),
        ])
        .map_err(|error| format!("diff of {sha} unreadable: {error}"))?;
        if diff
            .lines()
            .filter(|line| line.starts_with("diff --git "))
            .count()
            >= MAX_DIFF_FILES
        {
            return Err(format!(
                "diff of {sha} has at least {MAX_DIFF_FILES} files and may be truncated"
            ));
        }
        Ok(diff)
    }

    /// A single-parent commit is on the tip when a tip commit has its
    /// patch-id and the matching change is still present on the pinned tip.
    fn patch_on_tip(&mut self, sha: &str, commit: &Commit, tip: &Tip) -> Result<bool, String> {
        let diff = self.diff(sha)?;
        let Some(id) = crate::head_approval::patch_id(&diff)? else {
            return Ok(false);
        };
        let Some(path) = first_changed_path(&diff) else {
            return Ok(false);
        };
        let repo = self.repo;
        let listed = self.json(
            &[
                "api".to_owned(),
                format!(
                    "repos/{repo}/commits?sha={}&path={}&since={}&per_page={MAX_PATCH_CANDIDATES}",
                    tip.sha,
                    encode(&path),
                    encode(&commit.date)
                ),
                "--jq".to_owned(),
                "[.[] | select((.parents | length) == 1) | .sha]".to_owned(),
            ],
            "tip commits",
        )?;
        let candidates = listed.as_array().ok_or("tip commits are not a list")?;
        for candidate in candidates.iter().take(MAX_PATCH_CANDIDATES) {
            let candidate = candidate.as_str().ok_or("a tip commit has no sha")?;
            if candidate == sha {
                continue;
            }
            if crate::head_approval::patch_id(&self.diff(candidate)?)?.as_deref() == Some(&id) {
                let candidate = self.commit(candidate)?;
                let [parent] = candidate.parents.as_slice() else {
                    return Err("a matching tip commit is not single-parent".to_owned());
                };
                let parent = self.commit(parent)?;
                if self.same_as_tip(
                    Some(&candidate.tree),
                    Some(&parent.tree),
                    Some(&tip.tree),
                    0,
                )? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    /// Every path where `merge` differs from `first` is identical on `tip`.
    fn merge_on_tip(&mut self, merge: &str, first: &str, tip: &str) -> Result<bool, String> {
        self.same_as_tip(Some(merge), Some(first), Some(tip), 0)
    }

    fn same_as_tip(
        &mut self,
        merge: Option<&str>,
        first: Option<&str>,
        tip: Option<&str>,
        depth: usize,
    ) -> Result<bool, String> {
        if depth > MAX_DEPTH {
            return Err("tree walk is too deep".to_owned());
        }
        let merge = self.tree(merge)?;
        let first = self.tree(first)?;
        let tip = self.tree(tip)?;
        let names: BTreeSet<&String> = merge.keys().chain(first.keys()).collect();
        for name in names {
            let (m, f, t) = (merge.get(name), first.get(name), tip.get(name));
            if m == f || m == t {
                continue;
            }
            // `m` differs from the first parent and from the tip. A blob or an
            // absence cannot be excused; a tree is compared entry by entry.
            let Some(m) = m.filter(|entry| entry.kind == "tree") else {
                return Ok(false);
            };
            let sub = |entry: Option<&Entry>| {
                entry
                    .filter(|entry| entry.kind == "tree")
                    .map(|entry| entry.sha.clone())
            };
            if !self.same_as_tip(
                Some(&m.sha),
                sub(f).as_deref(),
                sub(t).as_deref(),
                depth + 1,
            )? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The entries of tree `sha`; an absent tree is empty.
    fn tree(&mut self, sha: Option<&str>) -> Result<Tree, String> {
        let Some(sha) = sha else {
            return Ok(Tree::new());
        };
        if let Some(tree) = self.trees.get(sha) {
            return Ok(tree.clone());
        }
        if self.tree_reads >= MAX_TREE_READS {
            return Err("more tree reads than the cap".to_owned());
        }
        self.tree_reads += 1;
        let repo = self.repo;
        let value = self.json(
            &[
                "api".to_owned(),
                format!("repos/{repo}/git/trees/{sha}"),
                "--jq".to_owned(),
                "{truncated: .truncated, tree: [.tree[] | {name: .path, mode: .mode, kind: \
                 .type, sha: .sha}]}"
                    .to_owned(),
            ],
            &format!("tree {sha}"),
        )?;
        if value.get("truncated").and_then(serde_json::Value::as_bool) != Some(false) {
            return Err(format!("tree {sha} is truncated or unmarked"));
        }
        let entries = value
            .get("tree")
            .and_then(|v| v.as_array())
            .ok_or_else(|| format!("tree {sha} has no entries"))?;
        let mut tree = Tree::new();
        for entry in entries {
            let name =
                text(entry, "name").ok_or_else(|| format!("tree {sha} entry has no name"))?;
            let parsed = Entry {
                mode: text(entry, "mode").ok_or_else(|| format!("tree {sha} entry has no mode"))?,
                kind: text(entry, "kind").ok_or_else(|| format!("tree {sha} entry has no type"))?,
                sha: text(entry, "sha").ok_or_else(|| format!("tree {sha} entry has no sha"))?,
            };
            tree.insert(name, parsed);
        }
        self.trees.insert(sha.to_owned(), tree.clone());
        Ok(tree)
    }
}

fn text(value: &serde_json::Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

/// The path on the first `diff --git a/X b/X` line, `None` when it is quoted
/// or cannot be split unambiguously.
fn first_changed_path(diff: &str) -> Option<String> {
    let line = diff
        .lines()
        .find_map(|line| line.strip_prefix("diff --git "))?;
    let rest = line.strip_prefix("a/")?;
    let (old, new) = rest.split_once(" b/")?;
    (old == new && !new.contains(" b/") && !new.contains('"')).then(|| new.to_owned())
}

/// Percent-encode everything but unreserved characters and `/`.
fn encode(value: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

#[cfg(test)]
mod tests;
