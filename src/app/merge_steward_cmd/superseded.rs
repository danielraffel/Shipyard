//! Name open pull requests whose change already landed another way (shadow).
//!
//! A pull request is reported superseded only on deterministic evidence:
//!
//! * `contained`: every commit of its head is already on the base
//!   (`compare <base>...<head>` is `ahead_by` 0).
//! * `tree_equal`: every file it changes is byte-identical on the base: each
//!   path it adds or modifies has the head's blob on the base, each path it
//!   removes is gone from the base, and each rename's old path is gone. The
//!   head's changes come from `compare <base>...<head>` (a list of 300 or
//!   more files proves nothing); the base tip's blob for each of those paths
//!   comes from one GraphQL read (`object(expression: "<base>:<path>")`).
//!   Blob identity only: a mode-only change is not seen here, which is why a
//!   close must still pass `pr-close-guard`'s tree-entry proof.
//! * `named_by`: a merged pull request in the same repository says
//!   `Supersedes #<n>` (GitHub records the mention as a cross-reference).
//! * `label_successor`: the pull request carries the `superseded` label and
//!   its body or a comment names `superseded by #<m>`, and `#<m>` merged.
//!
//! Titles, similar diffs and authorship are never evidence.
//!
//! This is shadow mode: the evidence is reported on the row
//! (`superseded`) and nothing is written to GitHub. Closing is a separate,
//! owner-approved capability (`docs/superseded-close.md`).
//!
//! A pull request that is merged, in the merge queue, or has an
//! `added_to_merge_queue` event with no later removal is never judged: a
//! head the queue is landing reads `contained` for a moment before GitHub
//! marks it merged.
//!
//! Cost: each open head is checked at most once per [`RECHECK_AFTER`]
//! (sooner only when its head moves), whatever the outcome (a failed read is
//! recorded as `unknown`, never as "not superseded"), so a blocked backlog
//! costs nothing on the passes in between. Per head and recheck: one
//! compare and, unless it is `contained`, one small GraphQL blob read, plus
//! its share of one GraphQL mention read per [`MENTION_BATCH`] heads: about
//! two calls per open head per 30 minutes. At most [`MAX_CHECKS_PER_PASS`]
//! heads per repository per pass; the pass budget is checked before each
//! head, and the first rate-limit error stops the detector for the pass. A pass that deferred rows for its budget
//! reads nothing here, and only `--apply` passes (which persist the ledger
//! cache) run it. `SHIPYARD_STEWARD_SUPERSEDED=off` turns it off.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How long one head's verdict is reused before it is read again.
pub(super) const RECHECK_AFTER: Duration = Duration::minutes(30);
/// Heads checked per repository per pass. A backlog is swept once, then each
/// head costs nothing until its head moves or [`RECHECK_AFTER`] passes.
pub(super) const MAX_CHECKS_PER_PASS: usize = 25;
/// GitHub's compare lists at most this many files; a list this long may be
/// cut short and proves nothing.
const COMPARE_FILE_CAP: usize = 300;
/// Pull requests per mention read.
pub(super) const MENTION_BATCH: usize = 10;
/// Cached verdicts are dropped after this long without a check.
const FORGET_AFTER: Duration = Duration::days(3);

/// The deterministic evidence that a pull request's change already landed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Evidence {
    /// `contained`, `tree_equal`, `named_by` or `label_successor`.
    pub kind: String,
    /// The merged pull request that carried the change, when one is named.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub successor: Option<u64>,
    /// The base commit the evidence was read against, when a compare read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Paths and blobs, or the naming text, behind the verdict.
    pub detail: String,
    /// What an owner would do. Shadow: Shipyard does not do it.
    pub action: String,
    /// Whether a future close-superseded could act on this evidence: only
    /// tree evidence (`contained`, `tree_equal`), on a pull request a fleet
    /// identity opened whose text closes no issue (`Fixes`/`Closes`/
    /// `Resolves`). Shadow: nothing closes either way.
    #[serde(default)]
    pub closeable: bool,
    /// Why it is not closeable, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_closeable: Option<String>,
}

/// One cached check of one head, in the steward ledger.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct Check {
    pub head: String,
    pub checked_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Evidence>,
    /// A read failed: no verdict either way; read again after the recheck
    /// interval, like any other check.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unknown: bool,
}

/// One file in a compare's `files` list.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct FileChange {
    pub path: String,
    pub previous: Option<String>,
    pub status: String,
    pub sha: Option<String>,
}

/// A pull request that mentioned this one (a cross-reference).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Mention {
    pub number: u64,
    pub merged: bool,
    pub same_repo: bool,
    /// The mentioning pull request's base branch.
    pub base_ref: String,
    pub text: String,
}

/// Whether the detector runs (`SHIPYARD_STEWARD_SUPERSEDED=off` stops it).
pub(super) fn enabled() -> bool {
    enabled_from(std::env::var("SHIPYARD_STEWARD_SUPERSEDED").ok().as_deref())
}

pub(super) fn enabled_from(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("off" | "0" | "false" | "no")
    )
}

/// Parse a compare's `files` array.
pub(super) fn parse_files(files: &Value) -> Option<Vec<FileChange>> {
    files
        .as_array()?
        .iter()
        .map(|file| {
            Some(FileChange {
                path: file.get("filename")?.as_str()?.to_owned(),
                previous: file
                    .get("previous_filename")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                status: file.get("status")?.as_str()?.to_owned(),
                sha: file
                    .get("sha")
                    .and_then(Value::as_str)
                    .filter(|sha| !sha.is_empty())
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn short(sha: &str) -> &str {
    sha.get(..8).unwrap_or(sha)
}

/// `tree_equal` evidence: every file `head` changed is identical on the base
/// tip. `base` maps each path to its blob on the base (`None`: absent; a path
/// missing from the map: unread, so no verdict). `None` when any differs, the
/// list may be truncated, or the head changes nothing (an empty diff is
/// `contained`, from the caller's `ahead_by` read, never here).
pub(super) fn tree_equal(
    head: &[FileChange],
    base: &BTreeMap<String, Option<String>>,
) -> Option<String> {
    if head.is_empty() || head.len() >= COMPARE_FILE_CAP {
        return None;
    }
    let absent = |path: &str| matches!(base.get(path), Some(None));
    let mut shown = Vec::new();
    for change in head {
        // `changed` (mode only), `copied` and `unchanged` prove nothing a blob
        // comparison can see: no verdict.
        if !matches!(
            change.status.as_str(),
            "added" | "modified" | "removed" | "renamed"
        ) {
            return None;
        }
        let same = if change.status == "removed" {
            absent(&change.path)
        } else {
            let sha = change.sha.as_deref()?;
            let landed = matches!(base.get(&change.path), Some(Some(b)) if b == sha);
            let old_gone = change
                .previous
                .as_deref()
                .filter(|old| change.status == "renamed" && *old != change.path)
                .is_none_or(absent);
            landed && old_gone
        };
        if !same {
            return None;
        }
        if shown.len() < 5 {
            shown.push(match change.sha.as_deref() {
                Some(sha) if change.status != "removed" => {
                    format!("{}@{}", change.path, short(sha))
                }
                _ => format!("{} (removed)", change.path),
            });
        }
    }
    let more = head.len().saturating_sub(shown.len());
    Some(format!(
        "{} file(s) identical on the base: {}{}",
        head.len(),
        shown.join(", "),
        if more > 0 {
            format!(" and {more} more")
        } else {
            String::new()
        }
    ))
}

/// Every path the base's blob is needed for: each changed path, and each
/// rename's old path.
pub(super) fn paths_to_read(head: &[FileChange]) -> Vec<String> {
    let mut paths: BTreeSet<String> = head.iter().map(|c| c.path.clone()).collect();
    paths.extend(
        head.iter()
            .filter(|c| c.status == "renamed")
            .filter_map(|c| c.previous.clone()),
    );
    paths.into_iter().collect()
}

fn graphql_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Each path's blob on the base tip; `None` when no file is there.
pub(super) type BaseBlobs = BTreeMap<String, Option<String>>;

/// The base tip's blob for each path (`None`: no file there; a tree or
/// submodule there reads as its type, which matches no blob), and the tip SHA.
pub(super) fn base_blobs(
    read: Read<'_>,
    repo: &str,
    base: &str,
    paths: &[String],
) -> Result<(BaseBlobs, Option<String>), String> {
    let fields = paths
        .iter()
        .enumerate()
        .map(|(i, path)| {
            format!(
                "f{i}: object(expression:{}){{__typename ... on Blob{{oid}}}}",
                graphql_string(&format!("{base}:{path}"))
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!(
        "query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{\
         tip: ref(qualifiedName:{}){{target{{oid}}}} root: object(expression:{}){{__typename}} \
         {fields}}}}}",
        graphql_string(&format!("refs/heads/{base}")),
        graphql_string(&format!("{base}:"))
    );
    let data = graphql(read, repo, &query)?;
    // The control: a null path means "absent" only when the same read
    // resolved the base and its root tree.
    let tip = data
        .pointer("/tip/target/oid")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("blob read could not resolve {base}"))?;
    if data.pointer("/root/__typename").and_then(Value::as_str) != Some("Tree") {
        return Err(format!("blob read could not resolve {base}'s root tree"));
    }
    let mut out = BTreeMap::new();
    for (i, path) in paths.iter().enumerate() {
        let node = data
            .get(format!("f{i}"))
            .ok_or_else(|| format!("blob read missing {path}"))?;
        let blob = if node.is_null() {
            None
        } else {
            match node.get("oid").and_then(Value::as_str) {
                Some(oid) => Some(oid.to_owned()),
                None => Some(format!(
                    "not-a-blob:{}",
                    node.get("__typename")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                )),
            }
        };
        out.insert(path.clone(), blob);
    }
    Ok((out, Some(tip)))
}

static SUPERSEDES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bsupersed(?:es|ing)\s*:?\s*((?:#\d+(?:\s*(?:,|and|&)\s*)?)+)").expect("regex")
});
static SUPERSEDED_BY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\bsuperseded\s+by\s+(?:pr\s+|pull request\s+)?#(\d+)\b").expect("regex")
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"#(\d+)").expect("regex"));
/// A qualifier just before the phrase that makes it not a full supersession.
static QUALIFIED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:not|never|partly|partially|partial|only\s+partly|does\s+not|doesn't|isn't|is\s+not)\s+(?:\w+\s+)?$")
        .expect("regex")
});

fn qualified(text: &str, start: usize) -> bool {
    let before = &text[..start];
    let from = before.char_indices().rev().nth(30).map_or(0, |(i, _)| i);
    QUALIFIED.is_match(&before[from..])
}

/// The numbers a text says it supersedes (`Supersedes #14, #15`).
pub(super) fn superseded_numbers(text: &str) -> BTreeSet<u64> {
    SUPERSEDES
        .captures_iter(text)
        .filter(|caps| !caps.get(0).is_some_and(|m| qualified(text, m.start())))
        .flat_map(|caps| {
            NUMBER
                .captures_iter(caps.get(1).map_or("", |m| m.as_str()))
                .filter_map(|n| n[1].parse().ok())
                .collect::<Vec<u64>>()
        })
        .collect()
}

/// The successors a pull request's own text names (`superseded by #16`).
pub(super) fn named_successors(texts: &[String]) -> BTreeSet<u64> {
    texts
        .iter()
        .flat_map(|text| {
            SUPERSEDED_BY
                .captures_iter(text)
                .filter(|caps| !caps.get(0).is_some_and(|m| qualified(text, m.start())))
                .filter_map(|caps| caps[1].parse().ok())
                .collect::<Vec<u64>>()
        })
        .collect()
}

/// `named_by` evidence: a merged pull request in the same repository that
/// says it supersedes `number`.
pub(super) fn named_by(number: u64, base: &str, mentions: &[Mention]) -> Option<(u64, String)> {
    mentions
        .iter()
        .filter(|m| m.merged && m.same_repo && m.number != number && m.base_ref == base)
        .filter(|m| !is_revert(&m.text))
        .find(|m| superseded_numbers(&m.text).contains(&number))
        .map(|m| {
            (
                m.number,
                format!("merged #{} says it supersedes #{number}", m.number),
            )
        })
}

/// A revert undoes rather than supersedes: its text never counts.
fn is_revert(text: &str) -> bool {
    let title = text.lines().next().unwrap_or("").trim_start();
    title.len() >= 6 && title[..6].eq_ignore_ascii_case("revert")
}

/// `label_successor` evidence: labelled `superseded`, names a successor,
/// and that successor merged into the same base (`merged` maps a number to
/// whether it merged into this pull request's base).
pub(super) fn label_successor(
    number: u64,
    labelled: bool,
    successors: &BTreeSet<u64>,
    merged: &BTreeMap<u64, bool>,
) -> Option<(u64, String)> {
    if !labelled {
        return None;
    }
    successors
        .iter()
        .copied()
        .filter(|successor| *successor != number)
        .find(|successor| merged.get(successor) == Some(&true))
        .map(|successor| {
            (
                successor,
                format!("labelled superseded; names #{successor}, which merged"),
            )
        })
}

fn action(successor: Option<u64>) -> String {
    successor.map_or_else(
        || {
            "close as superseded: its change is already on the base (shadow: Shipyard \
             does not close it)"
                .to_owned()
        },
        |m| format!("close as superseded by #{m} (shadow: Shipyard does not close it)"),
    )
}

pub(super) fn evidence(
    kind: &str,
    successor: Option<u64>,
    base_sha: Option<String>,
    detail: String,
) -> Evidence {
    let tree = matches!(kind, "contained" | "tree_equal");
    Evidence {
        kind: kind.to_owned(),
        successor,
        base_sha,
        detail,
        action: action(successor),
        closeable: tree,
        not_closeable: (!tree).then(|| "text evidence never closes".to_owned()),
    }
}

static CLOSING_KEYWORD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(?:close[sd]?|fix(?:e[sd])?|resolve[sd]?)\s*:?\s+(?:[\w.-]+/[\w.-]+)?#\d+")
        .expect("regex")
});

/// Whether a pull request body closes an issue on merge. Closing such a
/// pull request would leave that issue open without anyone deciding so.
pub(super) fn closes_issues(body: &str) -> bool {
    CLOSING_KEYWORD.is_match(body)
}

/// Mark tree evidence not closeable when the pull request closes an issue or
/// a non-fleet identity opened it.
pub(super) fn gate_close(mut found: Evidence, body: &str, fleet_author: bool) -> Evidence {
    if !found.closeable {
        return found;
    }
    let why = if closes_issues(body) {
        Some("its body closes an issue (Fixes/Closes/Resolves)")
    } else if !fleet_author {
        Some("a non-fleet author opened it")
    } else {
        None
    };
    if let Some(why) = why {
        found.closeable = false;
        found.not_closeable = Some(why.to_owned());
    }
    found
}

/// Which heads to check this pass: those with no cached verdict for their
/// current head first, then the stalest past [`RECHECK_AFTER`], at most
/// [`MAX_CHECKS_PER_PASS`].
pub(super) fn due(
    heads: &[(u64, String)],
    cache: &BTreeMap<String, Check>,
    repo: &str,
    now: DateTime<Utc>,
) -> Vec<u64> {
    let mut ranked: Vec<(Option<DateTime<Utc>>, u64)> = heads
        .iter()
        .filter_map(|(number, head)| {
            match cache.get(&key(repo, *number)).filter(|c| c.head == *head) {
                None => Some((None, *number)),
                Some(check) => {
                    let at = DateTime::parse_from_rfc3339(&check.checked_at)
                        .ok()
                        .map(|t| t.with_timezone(&Utc));
                    match at {
                        Some(at) if now - at < RECHECK_AFTER => None,
                        at => Some((at, *number)),
                    }
                }
            }
        })
        .collect();
    ranked.sort();
    ranked
        .into_iter()
        .take(MAX_CHECKS_PER_PASS)
        .map(|(_, n)| n)
        .collect()
}

pub(super) fn key(repo: &str, number: u64) -> String {
    format!("{}#{number}", repo.to_ascii_lowercase())
}

/// Drop cached verdicts for closed pull requests and stale entries.
pub(super) fn prune(
    cache: &mut BTreeMap<String, Check>,
    repo: &str,
    open: &BTreeSet<u64>,
    now: DateTime<Utc>,
) {
    let prefix = format!("{}#", repo.to_ascii_lowercase());
    cache.retain(|k, check| {
        let fresh = DateTime::parse_from_rfc3339(&check.checked_at)
            .is_ok_and(|t| now - t.with_timezone(&Utc) < FORGET_AFTER);
        match k.strip_prefix(&prefix) {
            Some(n) => fresh && n.parse().is_ok_and(|n: u64| open.contains(&n)),
            None => fresh,
        }
    });
}

/// A GitHub read: `gh` argv in, stdout out.
pub(super) type Read<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

fn read_json(read: Read<'_>, args: &[String]) -> Result<Value, String> {
    let raw = read(args)?;
    serde_json::from_str(&raw).map_err(|e| format!("malformed JSON: {e}"))
}

fn compare(read: Read<'_>, repo: &str, from: &str, to: &str) -> Result<Value, String> {
    read_json(
        read,
        &[
            "api".to_owned(),
            format!("repos/{repo}/compare/{from}...{to}"),
        ],
    )
}

/// Tree evidence for one head: `contained` or `tree_equal`, from one compare
/// against `base` and, unless it is `contained`, one blob read of its paths.
pub(super) fn tree_evidence(
    read: Read<'_>,
    repo: &str,
    base: &str,
    head: &str,
) -> Result<Option<Evidence>, String> {
    let forward = compare(read, repo, base, head)?;
    let base_sha = forward
        .pointer("/base_commit/sha")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if forward.get("ahead_by").and_then(Value::as_u64) == Some(0) {
        return Ok(Some(evidence(
            "contained",
            None,
            base_sha.clone(),
            format!(
                "every commit of head {} is already on {base} ({})",
                short(head),
                base_sha.as_deref().map_or("?", short)
            ),
        )));
    }
    let Some(head_files) = forward.get("files").and_then(parse_files) else {
        return Ok(None);
    };
    if head_files.is_empty() || head_files.len() >= COMPARE_FILE_CAP {
        return Ok(None);
    }
    let (blobs, tip) = base_blobs(read, repo, base, &paths_to_read(&head_files))?;
    let base_sha = tip.or(base_sha);
    Ok(tree_equal(&head_files, &blobs).map(|detail| {
        evidence(
            "tree_equal",
            None,
            base_sha.clone(),
            format!("{detail} (base {})", base_sha.as_deref().map_or("?", short)),
        )
    }))
}

/// What one batched GraphQL read says about each pull request: who
/// mentioned it, its own text, and whether it is labelled `superseded`.
#[derive(Debug, Default)]
pub(super) struct Mentions {
    pub by_pr: BTreeMap<u64, (Vec<Mention>, Vec<String>, bool)>,
    /// Merged, in the merge queue, or added to it with no later removal (or
    /// any of those unreadable): never judged.
    pub landing: BTreeSet<u64>,
    /// Each pull request's own body and whether a fleet identity (a GitHub
    /// App) opened it.
    pub own: BTreeMap<u64, (String, bool)>,
}

pub(super) fn mentions_query(numbers: &[u64]) -> String {
    let fields = numbers
        .iter()
        .map(|n| {
            format!(
                "p{n}: pullRequest(number:{n}){{ body merged isInMergeQueue author{{__typename}} \
                 labels(first:30){{nodes{{name}}}} comments(last:30){{nodes{{body authorAssociation}}}} \
                 queue: timelineItems(last:10,itemTypes:[ADDED_TO_MERGE_QUEUE_EVENT,\
                 REMOVED_FROM_MERGE_QUEUE_EVENT]){{nodes{{__typename}}}} \
                 timelineItems(last:40,itemTypes:[CROSS_REFERENCED_EVENT]){{nodes{{... on \
                 CrossReferencedEvent{{source{{... on PullRequest{{number merged baseRefName \
                 title body repository{{nameWithOwner}}}}}}}}}}}} }}"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{fields}}}}}"
    )
}

fn graphql(read: Read<'_>, repo: &str, query: &str) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| "invalid repository slug".to_owned())?;
    let value = read_json(
        read,
        &[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
            "-F".to_owned(),
            format!("owner={owner}"),
            "-F".to_owned(),
            format!("name={name}"),
        ],
    )?;
    if let Some(errors) = value.get("errors").filter(|e| !e.is_null()) {
        return Err(format!("graphql errors: {errors}"));
    }
    value
        .pointer("/data/repository")
        .cloned()
        .ok_or_else(|| "graphql: no repository".to_owned())
}

pub(super) fn parse_mentions(repo: &str, numbers: &[u64], data: &Value) -> Mentions {
    let mut out = Mentions::default();
    for n in numbers {
        let Some(node) = data.get(format!("p{n}")).filter(|v| !v.is_null()) else {
            continue;
        };
        let mentions = node
            .pointer("/timelineItems/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let source = item.get("source")?;
                Some(Mention {
                    number: source.get("number")?.as_u64()?,
                    merged: source.get("merged").and_then(Value::as_bool) == Some(true),
                    same_repo: source
                        .pointer("/repository/nameWithOwner")
                        .and_then(Value::as_str)
                        .is_some_and(|r| r.eq_ignore_ascii_case(repo)),
                    base_ref: source
                        .get("baseRefName")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                    text: format!(
                        "{}\n{}",
                        source.get("title").and_then(Value::as_str).unwrap_or(""),
                        source.get("body").and_then(Value::as_str).unwrap_or("")
                    ),
                })
            })
            .collect();
        let mut texts = vec![
            node.get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
        ];
        texts.extend(
            node.pointer("/comments/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                // Only a repository member's comment can name a successor.
                .filter(|c| {
                    matches!(
                        c.get("authorAssociation").and_then(Value::as_str),
                        Some("OWNER" | "MEMBER" | "COLLABORATOR")
                    )
                })
                .filter_map(|c| c.get("body").and_then(Value::as_str).map(str::to_owned)),
        );
        let labelled = node
            .pointer("/labels/nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|l| l.get("name").and_then(Value::as_str))
            .any(|l| l.eq_ignore_ascii_case(crate::merge_steward::SUPERSEDED_LABEL));
        let last_queue_event = node
            .pointer("/queue/nodes")
            .and_then(Value::as_array)
            .and_then(|nodes| nodes.last())
            .and_then(|e| e.get("__typename"))
            .and_then(Value::as_str);
        let landing = node.get("merged").and_then(Value::as_bool) != Some(false)
            || node.get("isInMergeQueue").and_then(Value::as_bool) != Some(false)
            || last_queue_event == Some("AddedToMergeQueueEvent");
        let body = node
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        let fleet_author =
            node.pointer("/author/__typename").and_then(Value::as_str) == Some("Bot");
        out.own.insert(*n, (body, fleet_author));
        out.by_pr.insert(*n, (mentions, texts, labelled));
        if landing {
            out.landing.insert(*n);
        }
    }
    out
}

/// Merged state of named successors not already known from mentions.
fn successors_merged(
    read: Read<'_>,
    repo: &str,
    base: &str,
    numbers: &BTreeSet<u64>,
) -> Result<BTreeMap<u64, bool>, String> {
    if numbers.is_empty() {
        return Ok(BTreeMap::new());
    }
    let fields = numbers
        .iter()
        .map(|n| format!("s{n}: pullRequest(number:{n}){{ merged baseRefName }}"))
        .collect::<Vec<_>>()
        .join(" ");
    let query = format!(
        "query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{{fields}}}}}"
    );
    let data = graphql(read, repo, &query)?;
    Ok(numbers
        .iter()
        .filter_map(|n| {
            let node = data.get(format!("s{n}"))?;
            let merged = node.get("merged").and_then(Value::as_bool)?;
            let same_base = node.get("baseRefName").and_then(Value::as_str) == Some(base);
            Some((*n, merged && same_base))
        })
        .collect())
}

/// What one repository's check found: each checked head's verdict
/// (`Err`: unknown), and the rate-limit error that stopped it, if any.
pub(super) type RepoVerdicts = (
    BTreeMap<u64, Result<Option<Evidence>, String>>,
    Option<String>,
);

/// Check the due heads of one repository. `stop` is the pass budget, asked
/// before every read; the first rate-limit error stops the check. A head
/// left unread has no entry, so it stays due.
pub(super) fn check_repo(
    read: Read<'_>,
    repo: &str,
    base: &str,
    heads: &[(u64, String)],
    stop: &dyn Fn() -> bool,
) -> RepoVerdicts {
    let mut out = BTreeMap::new();
    let limited = |error: &str| super::pass_budget::is_rate_limited(error);
    for chunk in heads.chunks(MENTION_BATCH) {
        if stop() {
            return (out, None);
        }
        let numbers: Vec<u64> = chunk.iter().map(|(n, _)| *n).collect();
        let mentions = match graphql(read, repo, &mentions_query(&numbers)) {
            Ok(data) => parse_mentions(repo, &numbers, &data),
            Err(error) if limited(&error) => return (out, Some(error)),
            Err(error) => {
                // Without the mention read neither the text evidence nor the
                // queue state is known: every head of the batch is unknown.
                for n in &numbers {
                    out.insert(*n, Err(format!("mention read failed: {error}")));
                }
                continue;
            }
        };
        let mut wanted = BTreeSet::new();
        for (refs, texts, labelled) in mentions.by_pr.values() {
            if *labelled {
                for s in named_successors(texts) {
                    if !refs.iter().any(|x| x.number == s && x.same_repo) {
                        wanted.insert(s);
                    }
                }
            }
        }
        let extra = match successors_merged(read, repo, base, &wanted) {
            Ok(extra) => extra,
            Err(error) if limited(&error) => return (out, Some(error)),
            Err(_) => BTreeMap::new(),
        };
        for (number, head) in chunk {
            let Some((refs, texts, labelled)) = mentions.by_pr.get(number) else {
                out.insert(
                    *number,
                    Err("pull request missing from the mention read".to_owned()),
                );
                continue;
            };
            if mentions.landing.contains(number) {
                // Merged or being landed by the queue: not this detector's.
                out.insert(*number, Ok(None));
                continue;
            }
            if let Some((m, detail)) = named_by(*number, base, refs) {
                out.insert(
                    *number,
                    Ok(Some(evidence("named_by", Some(m), None, detail))),
                );
                continue;
            }
            let mut merged: BTreeMap<u64, bool> = refs
                .iter()
                .filter(|x| x.same_repo)
                .map(|x| (x.number, x.merged && x.base_ref == base))
                .collect();
            merged.extend(extra.iter().map(|(k, v)| (*k, *v)));
            if let Some((m, detail)) =
                label_successor(*number, *labelled, &named_successors(texts), &merged)
            {
                out.insert(
                    *number,
                    Ok(Some(evidence("label_successor", Some(m), None, detail))),
                );
                continue;
            }
            if stop() {
                return (out, None);
            }
            match tree_evidence(read, repo, base, head) {
                Err(error) if limited(&error) => return (out, Some(error)),
                verdict => {
                    let (body, fleet) = mentions.own.get(number).cloned().unwrap_or_default();
                    out.insert(
                        *number,
                        verdict.map(|found| found.map(|f| gate_close(f, &body, fleet))),
                    );
                }
            }
        }
    }
    (out, None)
}

/// Attach cached or fresh evidence to every open row of one repository
/// (shadow: reads only). Queued, admitted and draft rows are skipped.
/// Returns the rate-limit error that stopped the check, for the pass's
/// rate-limit cooldown.
pub(super) fn annotate(
    actions: &super::GitHubActions,
    observation: &super::RepoObservation,
    ledger: &mut super::StewardLedger,
    reports: &mut [super::PrReport],
    now: DateTime<Utc>,
    stop: &dyn Fn() -> bool,
) -> Option<String> {
    let read = |args: &[String]| actions.run_gh(args).map_err(|e| e.to_string());
    annotate_with(
        &read,
        &observation.repo,
        &observation.base,
        ledger,
        reports,
        now,
        &|| !enabled() || stop(),
    )
}

pub(super) fn annotate_with(
    read: Read<'_>,
    repo: &str,
    base: &str,
    ledger: &mut super::StewardLedger,
    reports: &mut [super::PrReport],
    now: DateTime<Utc>,
    stop: &dyn Fn() -> bool,
) -> Option<String> {
    // A pass that spent its budget reads nothing more (pass_budget).
    if stop()
        || reports
            .iter()
            .any(|r| r.blocked.blocked_on == "deferred_pass_budget")
    {
        return None;
    }
    let skip = |token: &str| matches!(token, "queued" | "admitted" | "draft");
    let heads: Vec<(u64, String)> = reports
        .iter()
        .filter(|r| !skip(&r.blocked.blocked_on))
        .map(|r| (r.number, r.head_sha.clone()))
        .collect();
    let open: BTreeSet<u64> = reports.iter().map(|r| r.number).collect();
    let cache = &mut ledger.superseded_checks;
    prune(cache, repo, &open, now);
    let due_numbers = due(&heads, cache, repo, now);
    let due_heads: Vec<(u64, String)> = heads
        .iter()
        .filter(|(n, _)| due_numbers.contains(n))
        .cloned()
        .collect();
    let (verdicts, rate_limited) = check_repo(read, repo, base, &due_heads, stop);
    for (number, found) in verdicts {
        if let Some((_, head)) = due_heads.iter().find(|(n, _)| *n == number) {
            cache.insert(
                key(repo, number),
                Check {
                    head: head.clone(),
                    checked_at: now.to_rfc3339(),
                    unknown: found.is_err(),
                    evidence: found.ok().flatten(),
                },
            );
        }
    }
    for report in reports.iter_mut() {
        if skip(&report.blocked.blocked_on) {
            continue;
        }
        report.blocked.superseded = cache
            .get(&key(repo, report.number))
            .filter(|c| c.head == report.head_sha)
            .and_then(|c| c.evidence.clone());
    }
    rate_limited
}
