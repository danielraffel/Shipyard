//! Which pull request a shared commit belongs to, by which branch received it
//! first.
//!
//! When P carries commits that another open pull request Q introduced (they
//! are in Q's own diff), the commit graph cannot say whether Q branched off
//! P's line or P took Q's commits: the two graphs are the same. What differs
//! is the order the commits reached the two branches, and GitHub records that
//! itself: the repository activity log
//! (`GET /repos/{owner}/{repo}/activity?ref=refs/heads/<branch>`) lists every
//! push, force-push and branch creation with the server's timestamp and the
//! sha the branch moved to. A shared commit C is P's own when P's branch held
//! C strictly before Q's branch did. Then Q built on P's work (the stacked
//! child that targets the base) and C is reviewed under P's exact-head
//! approval. Otherwise C came to P from Q (P merged Q, was reset or
//! fast-forwarded onto Q, or cherry-picked the same sha), and C stays Q's:
//! unapproved, it refuses P whatever P's approval says.
//!
//! When a branch first held C is read from the log in order, one compare
//! (`base...after`, cached for the pass) per entry, until every shared commit
//! is placed. Fail-closed throughout: an unreadable log, a compare over the
//! cap, more than [`MAX_PUSHES_READ`] entries, a cross-repository pull
//! request (whose branch is not in this repository's log), or a commit not
//! found on P's branch leaves the commit Q's. Q's log must also be whole and
//! current: it starts at a creation no later than Q was opened (a later one
//! is a renamed or recreated branch), every entry starts where the previous
//! one left the branch (`before` equals the previous `after`), its last entry
//! is Q's listed head, and a shared commit the walk did not find on Q's
//! branch must be explained by entries after P's arrival; otherwise the log
//! is incomplete and nothing is settled.
//!
//! P's log must be unbroken as well, and its last deletion must be followed
//! by a creation, so a missing entry cannot credit an earlier life. Each log
//! is read as one page, newest first, with one entry more than
//! [`MAX_PUSHES_READ`]: a full page is refused after a single read. An
//! over-cap Q (its own diff over the cap) is never judged here; it clears
//! only through its approval.
//!
//! Ownership follows push order, not intent: a child pushed before its
//! parent, or in the same second, owns the shared commits. For P only
//! the branch's current life counts (entries from its last creation), so a
//! branch name reused after a deletion cannot claim an earlier life's
//! commits; for Q every entry counts, which can only make Q earlier.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::{CommitGraph, GraphCache, OpenPr, cached_graph};
use crate::head_approval::RunGh;

/// Most log entries read per branch.
pub(super) const MAX_PUSHES_READ: usize = 60;

/// One entry of a branch's activity log.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Push {
    pub at: DateTime<Utc>,
    pub after: String,
    pub creation: bool,
}

/// One branch's activity log: its entries (deletions dropped), oldest first,
/// and the first break in its chain of `before` and `after` shas, if any.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Activity {
    pub pushes: Vec<Push>,
    pub gap: Option<String>,
    /// When the branch was last deleted, if the log shows a deletion.
    pub last_deletion: Option<DateTime<Utc>>,
}

/// Activity logs read this pass, by branch.
pub(super) type ActivityCache = BTreeMap<String, Result<Activity, String>>;

/// Percent-encode a branch name for a query string.
fn encode(branch: &str) -> String {
    branch
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"/._-".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// `branch`'s activity log, oldest first, with deletions dropped. Every
/// entry but a creation must start where the previous entry left the branch
/// (`before` equals the previous `after`); the first entry that does not is
/// recorded as the log's gap.
fn read_activity(run_gh: RunGh<'_>, repo: &str, branch: &str) -> Result<Activity, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        // One page, newest first, one entry over the cap: a log with more
        // entries than the cap is refused after a single read, never paged.
        format!(
            "repos/{repo}/activity?ref=refs/heads/{}&direction=desc&per_page={}",
            encode(branch),
            MAX_PUSHES_READ + 1
        ),
        "--jq".to_owned(),
        ".[] | {at: .timestamp, kind: .activity_type, before: .before, after: .after}".to_owned(),
    ])
    .map_err(|error| format!("activity of {branch} unreadable: {error}"))?;
    let mut entries: Vec<&str> = raw.lines().filter(|line| !line.trim().is_empty()).collect();
    if entries.len() > MAX_PUSHES_READ {
        return Err(format!(
            "activity of {branch} has more than {MAX_PUSHES_READ} entries, over the read cap"
        ));
    }
    // The page is newest first; walk it oldest first.
    entries.reverse();
    let mut last_deletion = None;
    let mut pushes = Vec::new();
    let mut gap = None;
    let mut previous: Option<String> = None;
    for line in entries {
        let value: serde_json::Value = serde_json::from_str(line)
            .map_err(|error| format!("activity of {branch} malformed: {error}"))?;
        let field = |name: &str| {
            value
                .get(name)
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| format!("activity of {branch} has an entry without {name}"))
        };
        let kind = field("kind")?;
        let after = field("after")?.to_ascii_lowercase();
        let before = value
            .get("before")
            .and_then(serde_json::Value::as_str)
            .map(str::to_ascii_lowercase);
        if kind != "branch_creation"
            && gap.is_none()
            && previous
                .as_ref()
                .is_some_and(|previous| before.as_ref() != Some(previous))
        {
            gap = Some(format!(
                "the activity log of {branch} skips from {} to {}",
                previous.as_deref().unwrap_or_default(),
                before.as_deref().unwrap_or("an entry without before")
            ));
        }
        previous = Some(after.clone());
        let at = field("at")?
            .parse::<DateTime<Utc>>()
            .map_err(|error| format!("activity of {branch}: bad time: {error}"))?;
        if kind == "branch_deletion" {
            last_deletion = Some(at);
            continue;
        }
        if after.bytes().all(|byte| byte == b'0') {
            continue;
        }
        pushes.push(Push {
            at,
            after,
            creation: kind == "branch_creation",
        });
    }
    // A stable sort: entries in the same second keep the log's order.
    pushes.sort_by_key(|push| push.at);
    Ok(Activity {
        pushes,
        gap,
        last_deletion,
    })
}

/// When `pushes` first reached each of `shas` (by `base...after`), for those
/// it reached. Stops once every sha is placed or the log passes `until`.
fn first_reached(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    pushes: &[Push],
    shas: &BTreeSet<String>,
    until: Option<DateTime<Utc>>,
    graphs: &mut GraphCache,
) -> Result<BTreeMap<String, DateTime<Utc>>, String> {
    let mut reached = BTreeMap::new();
    for push in pushes {
        if reached.len() == shas.len() || until.is_some_and(|until| push.at > until) {
            break;
        }
        match cached_graph(run_gh, repo, graphs, base, &push.after)? {
            CommitGraph::Complete(graph) => {
                for sha in shas {
                    if graph.contains_key(sha) {
                        reached.entry(sha.clone()).or_insert(push.at);
                    }
                }
            }
            CommitGraph::OverCap { total } => {
                return Err(format!(
                    "compare {base}...{} has {total} commits, over the read cap",
                    push.after
                ));
            }
        }
    }
    Ok(reached)
}

/// The commits of `shared` that are P's own: P's branch (`p_branch`, current
/// life only) held each strictly before `q`'s branch did. `Err` when that
/// cannot be settled, which the caller treats as "none are P's".
#[allow(clippy::too_many_arguments)]
pub(super) fn p_owned(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    p_branch: &str,
    p_cross_repository: bool,
    q: &OpenPr,
    shared: &[String],
    activity: &mut ActivityCache,
    graphs: &mut GraphCache,
) -> Result<BTreeSet<String>, String> {
    if p_cross_repository || q.cross_repository {
        return Err(
            "a cross-repository branch is not in this repository's activity log".to_owned(),
        );
    }
    let mut log = |branch: &str| {
        activity
            .entry(branch.to_owned())
            .or_insert_with(|| read_activity(run_gh, repo, branch))
            .clone()
    };
    // P's log must be unbroken too: a missing entry could hide a deletion
    // and re-creation, crediting P with an earlier life's commits.
    let p_activity = log(p_branch)?;
    if let Some(gap) = p_activity.gap {
        return Err(gap);
    }
    let p_log = p_activity.pushes;
    let q_activity = log(&q.head_ref)?;
    // Q's whole history must be in view and unbroken: a log that does not
    // start at a branch creation, skips an entry, or has not caught up with
    // Q's listed head may have lost the entry where Q first held a commit.
    if let Some(gap) = q_activity.gap {
        return Err(gap);
    }
    let q_log = q_activity.pushes;
    let Some(q_created) = q_log.first().filter(|push| push.creation) else {
        return Err(format!(
            "the activity log of {} does not reach back to its creation",
            q.head_ref
        ));
    };
    if q_log
        .last()
        .is_none_or(|push| push.after != q.head_sha.to_ascii_lowercase())
    {
        return Err(format!(
            "the activity log of {} has not reached #{}'s head {}",
            q.head_ref, q.number, q.head_sha
        ));
    }
    // A branch made after its pull request was opened is a renamed or
    // recreated one, whose earlier name's history is not in this log.
    match q.created_at {
        Some(opened) if q_created.at <= opened => {}
        Some(opened) => {
            return Err(format!(
                "{} was created at {}, after #{} was opened at {opened}: renamed or recreated",
                q.head_ref, q_created.at, q.number
            ));
        }
        None => return Err(format!("#{} has no listed opening time", q.number)),
    }
    // P's branch in its current life: from its last creation, which must
    // follow its last deletion (an open pull request's branch exists).
    let p_life = p_log
        .iter()
        .rposition(|push| push.creation)
        .map_or(&p_log[..], |start| &p_log[start..]);
    if let Some(deleted) = p_activity.last_deletion
        && !p_life
            .first()
            .is_some_and(|push| push.creation && push.at >= deleted)
    {
        return Err(format!(
            "the activity log of {p_branch} shows a deletion at {deleted} with no creation after it"
        ));
    }
    let shas: BTreeSet<String> = shared.iter().cloned().collect();
    let on_p = first_reached(run_gh, repo, base, p_life, &shas, None, graphs)?;
    let latest = on_p.values().max().copied();
    let on_q = first_reached(run_gh, repo, base, &q_log, &shas, latest, graphs)?;
    // Q's head holds every shared commit (they are in Q's own diff), so a
    // commit the walk did not find on Q's branch must arrive in an entry
    // after `latest` that the walk did not read. With no such entry the log
    // contradicts Q's diff: it is incomplete, and nothing can be settled.
    let read_on = latest.is_some_and(|latest| q_log.iter().any(|push| push.at > latest));
    let mut owned = BTreeSet::new();
    for (sha, p_at) in on_p {
        match on_q.get(&sha) {
            Some(q_at) if p_at < *q_at => {
                owned.insert(sha);
            }
            Some(_) => {}
            None if read_on => {
                owned.insert(sha);
            }
            None => {
                return Err(format!(
                    "the activity log of {} does not show it holding {sha}",
                    q.head_ref
                ));
            }
        }
    }
    Ok(owned)
}

#[cfg(test)]
mod tests;
