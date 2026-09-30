//! The opt-in sticky pull-request comment.
//!
//! One comment per pull request, identified by [`COMMENT_MARKER`] *and* by
//! being ours: either its id is recorded in the ledger, or its author is the
//! configured bot login. A comment carrying the marker that someone else wrote
//! is never edited. The comment is created once, patched only when the
//! rendered body changes (edits do not notify), and rewritten to "resolved"
//! when every flag clears. Nothing is ever deleted.
//!
//! The only requests this module can send are `POST
//! repos/{repo}/issues/{n}/comments` and `PATCH
//! repos/{repo}/issues/comments/{id}`, and only through the writer passed to
//! [`apply`]. Planning ([`plan`]) is read-only.

use std::collections::BTreeMap;

use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

use super::COMMENT_MARKER;
use super::flags::{Flag, FlagKind};
use super::ledger::{CommentRecord, Ledger, body_sha256};
use crate::gate_cost::{SyncGhReader, read_pages};

/// Render the body for a pull request's current flags. `None` when there is
/// nothing to say (no flags, or only the split advisory).
#[must_use]
pub fn render(flags: &[&Flag]) -> Option<String> {
    if flags
        .iter()
        .all(|flag| flag.kind == FlagKind::SplitCandidate)
    {
        return None;
    }
    let mut body = format!(
        "{COMMENT_MARKER}\n**Shipyard PR watch**: read-only observations about this pull request's CI. \
         Nothing here was changed automatically.\n\n"
    );
    for flag in flags {
        let _ = writeln!(
            body,
            "- **{}** (flag {}, `{}`): {}",
            flag.verdict,
            flag.kind.number(),
            flag.kind.as_str(),
            flag.evidence
        );
    }
    let _ = writeln!(
        body,
        "\n_Clears when the condition stops holding or a new head is pushed. Add the `{}` label to acknowledge._",
        super::ACK_LABEL
    );
    Some(body)
}

/// Body written when every flag has cleared.
#[must_use]
pub fn resolved_body() -> String {
    format!(
        "{COMMENT_MARKER}\n**Shipyard PR watch**: resolved. No flags hold on this pull request.\n"
    )
}

/// One planned comment request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum CommentAction {
    /// POST a new comment.
    Create {
        /// Pull request.
        pr: u64,
        /// Body.
        body: String,
    },
    /// PATCH our existing comment.
    Update {
        /// Pull request.
        pr: u64,
        /// Comment id.
        comment_id: u64,
        /// Body.
        body: String,
    },
}

impl CommentAction {
    /// The exact `gh` argv this action sends.
    #[must_use]
    pub fn argv(&self, repo: &str) -> Vec<String> {
        match self {
            Self::Create { pr, body } => vec![
                "api".to_owned(),
                "--method".to_owned(),
                "POST".to_owned(),
                format!("repos/{repo}/issues/{pr}/comments"),
                "-f".to_owned(),
                format!("body={body}"),
            ],
            Self::Update {
                comment_id, body, ..
            } => vec![
                "api".to_owned(),
                "--method".to_owned(),
                "PATCH".to_owned(),
                format!("repos/{repo}/issues/comments/{comment_id}"),
                "-f".to_owned(),
                format!("body={body}"),
            ],
        }
    }
}

/// Find this tool's comment on a pull request by listing its comments.
/// `Err` when the list is unreadable, which callers treat as "unknown, do not
/// create" rather than "absent".
fn find_ours(
    gh: &SyncGhReader<'_>,
    repo: &str,
    pr: u64,
    author: Option<&str>,
) -> Result<Option<(u64, String)>, String> {
    let Some(author) = author else {
        return Ok(None);
    };
    let pages = read_pages(
        gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        &["per_page=100".to_owned()],
    )?;
    for comment in pages
        .iter()
        .flat_map(|page| page.as_array().cloned().unwrap_or_default())
    {
        let body = comment
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let login = comment
            .pointer("/user/login")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if body.contains(COMMENT_MARKER)
            && login.eq_ignore_ascii_case(author)
            && let Some(id) = comment.get("id").and_then(Value::as_u64)
        {
            return Ok(Some((id, body.to_owned())));
        }
    }
    Ok(None)
}

/// Plan the comment requests for this scan. Reads comment lists only for pull
/// requests that have flags and no recorded comment.
#[must_use]
pub fn plan(
    gh: &SyncGhReader<'_>,
    ledger: &mut Ledger,
    flags: &[Flag],
    open_prs: &[u64],
    author: Option<&str>,
) -> (Vec<CommentAction>, Vec<String>) {
    let mut by_pr: BTreeMap<u64, Vec<&Flag>> = BTreeMap::new();
    for flag in flags {
        by_pr.entry(flag.pr).or_default().push(flag);
    }
    let mut actions = Vec::new();
    let mut gaps = Vec::new();
    let mut prs: Vec<u64> = by_pr.keys().copied().collect();
    prs.extend(ledger.comments.keys().copied());
    prs.sort_unstable();
    prs.dedup();
    for pr in prs {
        let desired = by_pr.get(&pr).and_then(|flags| render(flags));
        if let Some(record) = ledger.comments.get(&pr) {
            let body = desired.unwrap_or_else(resolved_body);
            if body_sha256(&body) != record.body_sha256 {
                actions.push(CommentAction::Update {
                    pr,
                    comment_id: record.comment_id,
                    body,
                });
            }
        } else {
            let Some(body) = desired else {
                continue;
            };
            if !open_prs.contains(&pr) {
                continue;
            }
            match find_ours(gh, &ledger.repo, pr, author) {
                Ok(Some((comment_id, existing))) => {
                    ledger.comments.insert(
                        pr,
                        CommentRecord {
                            comment_id,
                            body_sha256: body_sha256(&existing),
                        },
                    );
                    if existing != body {
                        actions.push(CommentAction::Update {
                            pr,
                            comment_id,
                            body,
                        });
                    }
                }
                Ok(None) => actions.push(CommentAction::Create { pr, body }),
                Err(error) => gaps.push(format!(
                    "#{pr}: comments unreadable, not commenting: {error}"
                )),
            }
        }
    }
    (actions, gaps)
}

/// Send planned requests through `write` and record what was written.
pub fn apply(
    ledger: &mut Ledger,
    actions: &[CommentAction],
    write: &dyn Fn(&[String]) -> Result<String, String>,
) -> Vec<String> {
    let mut errors = Vec::new();
    for action in actions {
        let argv = action.argv(&ledger.repo);
        match (action, write(&argv)) {
            (CommentAction::Create { pr, body }, Ok(response)) => {
                let id = serde_json::from_str::<Value>(&response)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_u64));
                match id {
                    Some(comment_id) => {
                        ledger.comments.insert(
                            *pr,
                            CommentRecord {
                                comment_id,
                                body_sha256: body_sha256(body),
                            },
                        );
                    }
                    None => errors.push(format!("#{pr}: created comment returned no id")),
                }
            }
            (CommentAction::Update { pr, body, .. }, Ok(_)) => {
                if let Some(record) = ledger.comments.get_mut(pr) {
                    record.body_sha256 = body_sha256(body);
                }
            }
            (CommentAction::Create { pr, .. } | CommentAction::Update { pr, .. }, Err(error)) => {
                if matches!(action, CommentAction::Update { .. })
                    && (error.contains("404") || error.contains("Not Found"))
                {
                    // Our comment was deleted by someone; forget it.
                    ledger.comments.remove(pr);
                }
                errors.push(format!("#{pr}: {error}"));
            }
        }
    }
    errors
}
