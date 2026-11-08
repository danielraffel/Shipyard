//! Batched current-head check reads for the steward census.
//!
//! The census needs the complete check set of every head whose checks can
//! decide something. Read over REST, that is a `check-runs` read (one page per
//! 100 runs) plus a `statuses` read per head, every pass, each a separate `gh`
//! child. Here one GraphQL query reads the `statusCheckRollup` of up to
//! [`HEADS_PER_QUERY`] heads at once, and a head's commit-status history is
//! read over REST only when the rollup shows it may have changed.
//!
//! The hydrated set is the one the REST path builds, field for field:
//!
//! * Check runs come from the rollup. It lists the same latest run per check
//!   suite and name as `check-runs` (whose default filter is `latest`), with
//!   the same database id, details URL, timestamps and producing app. Status
//!   and conclusion are lowercased to the REST spelling.
//! * Commit statuses always come from the REST history, never from the
//!   rollup. The rollup carries only the newest status per context, and the
//!   steward reads more than that: the handoff check asks whether any status
//!   of its context succeeded, the rerun budget and the blocked-on report walk
//!   every observation, and an app-bound rule may be satisfied by an older
//!   status from its app under a newer one from someone else.
//!
//! The history is append-only: a new status on a head is always the newest of
//! its context. So a history read on an earlier pass is still complete while
//! the newest entry of every context in it matches what the rollup reports now
//! (same context, state, time, target and creator), and the rollup names no
//! other context. Such a history is reused from [`StatusHistory`]; anything
//! else, including a status the rollup cannot key, reads REST again.
//!
//! Any rollup failure (an error, a missing head, a malformed node, more than
//! [`MAX_PAGES`] pages) leaves the batch to the per-head REST path, so the
//! census fails closed exactly where it did before.
//!
//! The batch stays small on purpose. The bulk open-pull-request list dropped
//! `statusCheckRollup` because one rollup per row exceeded GitHub's per-query
//! limits past about 30 pull requests; a few heads per query stay far below
//! them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{GitHubActions, StewardCheck, Value};

/// Heads read by one rollup query.
pub(super) const HEADS_PER_QUERY: usize = 6;
/// Rollup contexts asked for per head and page; GitHub's maximum.
const CONTEXTS_PER_PAGE: usize = 100;
/// Pages read per head before the rollup is refused as partial, matching the
/// REST path's 1,000-row cap.
pub(super) const MAX_PAGES: usize = 10;

const CONTEXT_FIELDS: &str = "__typename ... on CheckRun{name status conclusion databaseId detailsUrl startedAt completedAt checkSuite{app{databaseId}}} ... on StatusContext{context state targetUrl createdAt creator{__typename login ... on Bot{databaseId} ... on User{databaseId}}}";

/// What the newest status of one context says, in a form both the rollup and
/// a REST history row produce.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) struct StatusKey {
    context: String,
    state: String,
    created_at: Option<String>,
    target_url: Option<String>,
    creator_login: Option<String>,
    creator_id: Option<u64>,
}

/// One head's rollup.
#[derive(Clone, Debug, Default)]
pub(super) struct HeadRollup {
    /// Check runs, parsed as the REST path parses them.
    pub(super) check_runs: Vec<StewardCheck>,
    /// The newest status of each context the rollup reports.
    pub(super) latest_statuses: BTreeSet<StatusKey>,
    /// Whether the rollup reports any commit status at all.
    pub(super) has_statuses: bool,
    /// Whether some reported status could not be keyed, so no cached history
    /// can be shown current.
    pub(super) unkeyable_status: bool,
}

impl HeadRollup {
    /// Whether `rows`, a REST status history read earlier, is still this
    /// head's complete history.
    pub(super) fn history_is_current(&self, rows: &[Value]) -> bool {
        !self.unkeyable_status && latest_status_keys(rows).as_ref() == Some(&self.latest_statuses)
    }
}

/// The newest status per context of a REST history, keyed. `None` when a row
/// lacks the id or context that ordering needs.
pub(super) fn latest_status_keys(rows: &[Value]) -> Option<BTreeSet<StatusKey>> {
    let mut newest: BTreeMap<String, (u64, &Value)> = BTreeMap::new();
    for row in rows {
        let context = row.get("context").and_then(Value::as_str)?;
        let id = row.get("id").and_then(Value::as_u64)?;
        let slot = newest.entry(context.to_owned()).or_insert((id, row));
        if id > slot.0 {
            *slot = (id, row);
        }
    }
    newest
        .into_iter()
        .map(|(context, (_, row))| {
            Some(StatusKey {
                context,
                state: row
                    .get("state")
                    .and_then(Value::as_str)?
                    .to_ascii_uppercase(),
                created_at: row
                    .get("created_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                target_url: row
                    .get("target_url")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                creator_login: row
                    .pointer("/creator/login")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                creator_id: row.pointer("/creator/id").and_then(Value::as_u64),
            })
        })
        .collect()
}

/// A rollup status context, keyed like a REST row. GraphQL names a bot
/// without the `[bot]` suffix REST gives its login.
fn rollup_status_key(node: &Value) -> Option<StatusKey> {
    let creator = node.get("creator").filter(|creator| !creator.is_null());
    let creator_login = match creator {
        None => None,
        Some(creator) => {
            let login = creator.get("login").and_then(Value::as_str)?;
            Some(match creator.get("__typename").and_then(Value::as_str) {
                Some("Bot") => format!("{login}[bot]"),
                Some("User") => login.to_owned(),
                _ => return None,
            })
        }
    };
    let creator_id = match creator {
        None => None,
        Some(creator) => Some(creator.get("databaseId").and_then(Value::as_u64)?),
    };
    let state = node.get("state").and_then(Value::as_str)?;
    // REST has no `expected` state; such a context cannot match any history.
    if state.eq_ignore_ascii_case("EXPECTED") {
        return None;
    }
    Some(StatusKey {
        context: node.get("context").and_then(Value::as_str)?.to_owned(),
        state: state.to_ascii_uppercase(),
        created_at: node
            .get("createdAt")
            .and_then(Value::as_str)
            .map(str::to_owned),
        target_url: node
            .get("targetUrl")
            .and_then(Value::as_str)
            .map(str::to_owned),
        creator_login,
        creator_id,
    })
}

/// A rollup check run, as `parse_rest_check` reads the same run.
fn rollup_check_run(node: &Value) -> Option<StewardCheck> {
    let mut check = super::observation::parse_check(node)?;
    check.status = check.status.to_ascii_lowercase();
    check.conclusion = check
        .conclusion
        .map(|conclusion| conclusion.to_ascii_lowercase());
    check.check_id = node.get("databaseId").and_then(Value::as_u64);
    Some(check)
}

fn rollup_query(heads: &[(String, Option<String>)]) -> String {
    use std::fmt::Write as _;
    let mut variables = String::from("$owner:String!,$name:String!");
    let mut selections = String::new();
    for (index, (_, cursor)) in heads.iter().enumerate() {
        let _ = write!(variables, ",$oid{index}:GitObjectID!");
        let after = if cursor.is_some() {
            let _ = write!(variables, ",$after{index}:String");
            format!(",after:$after{index}")
        } else {
            String::new()
        };
        let _ = write!(
            selections,
            "checkRollup{index}:object(oid:$oid{index}){{__typename ... on Commit{{oid statusCheckRollup{{contexts(first:{CONTEXTS_PER_PAGE}{after}){{pageInfo{{hasNextPage endCursor}} nodes{{{CONTEXT_FIELDS}}}}}}}}}}}"
        );
    }
    format!("query({variables}){{repository(owner:$owner,name:$name){{{selections}}}}}")
}

/// Read the rollup of every head in `heads` (full SHAs), following each
/// head's pages. Every head is answered, or the whole read is refused.
pub(super) fn read_rollups(
    actions: &GitHubActions,
    repo: &str,
    heads: &[String],
    deadline: Option<Instant>,
) -> Result<BTreeMap<String, HeadRollup>, String> {
    // `gh api graphql` has no `--repo`; scope the client instead, as the
    // merge-queue read does.
    let actions = actions.clone().with_repo_override(repo);
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository slug `{repo}`"))?;
    let mut rollups: BTreeMap<String, HeadRollup> = heads
        .iter()
        .map(|head| (head.clone(), HeadRollup::default()))
        .collect();
    let mut seen_cursors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut pending: Vec<(String, Option<String>)> =
        rollups.keys().map(|head| (head.clone(), None)).collect();
    for _ in 0..MAX_PAGES {
        if pending.is_empty() {
            return Ok(rollups);
        }
        // Every value is passed with `-f`: `-F` would turn an all-digit SHA
        // into a number.
        let mut args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={}", rollup_query(&pending)),
            "-f".to_owned(),
            format!("owner={owner}"),
            "-f".to_owned(),
            format!("name={name}"),
        ];
        for (index, (head, cursor)) in pending.iter().enumerate() {
            args.extend(["-f".to_owned(), format!("oid{index}={head}")]);
            if let Some(cursor) = cursor {
                args.extend(["-f".to_owned(), format!("after{index}={cursor}")]);
            }
        }
        let value = super::observation::gh_json_with_deadline(
            &actions,
            &args,
            "current-head check rollup",
            deadline,
        )?;
        if value
            .get("errors")
            .is_some_and(|errors| !errors.is_null() && errors != &Value::Array(Vec::new()))
        {
            return Err(format!("current-head check rollup GraphQL errors: {value}"));
        }
        let mut next = Vec::new();
        for (index, (head, _)) in pending.iter().enumerate() {
            let node = value
                .pointer(&format!("/data/repository/checkRollup{index}"))
                .filter(|node| !node.is_null())
                .ok_or_else(|| format!("current-head check rollup has no commit {head}"))?;
            let rollup = rollups
                .get_mut(head)
                .expect("every pending head has a rollup");
            if let Some(cursor) = absorb_page(rollup, head, node)? {
                if !seen_cursors
                    .entry(head.clone())
                    .or_default()
                    .insert(cursor.clone())
                {
                    return Err(format!(
                        "current-head check rollup of {head} repeated a cursor"
                    ));
                }
                next.push((head.clone(), Some(cursor)));
            }
        }
        pending = next;
    }
    if pending.is_empty() {
        Ok(rollups)
    } else {
        Err(format!(
            "current-head check rollup exceeds {} contexts; refusing a partial check scan",
            MAX_PAGES * CONTEXTS_PER_PAGE
        ))
    }
}

/// Add one page of `head`'s rollup (`node`, the commit object) to `rollup`.
/// Returns the cursor of the next page, if there is one.
fn absorb_page(
    rollup: &mut HeadRollup,
    head: &str,
    node: &Value,
) -> Result<Option<String>, String> {
    if node.get("__typename").and_then(Value::as_str) != Some("Commit")
        || !node
            .get("oid")
            .and_then(Value::as_str)
            .is_some_and(|oid| oid.eq_ignore_ascii_case(head))
    {
        return Err(format!(
            "current-head check rollup answered {head} with another object"
        ));
    }
    let Some(contexts) = node
        .get("statusCheckRollup")
        .filter(|rollup| !rollup.is_null())
        .map(|rollup| rollup.get("contexts"))
    else {
        // A head with no check run and no status has no rollup.
        return Ok(None);
    };
    let contexts =
        contexts.ok_or_else(|| format!("current-head check rollup of {head} has no contexts"))?;
    let nodes = contexts
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("current-head check rollup of {head} has no nodes"))?;
    for context in nodes {
        match context.get("__typename").and_then(Value::as_str) {
            Some("CheckRun") => {
                let check = rollup_check_run(context)
                    .ok_or_else(|| format!("malformed check run in the rollup of {head}"))?;
                rollup.check_runs.push(check);
            }
            Some("StatusContext") => {
                rollup.has_statuses = true;
                match rollup_status_key(context) {
                    Some(key) => {
                        rollup.latest_statuses.insert(key);
                    }
                    None => rollup.unkeyable_status = true,
                }
            }
            _ => return Err(format!("unknown context kind in the rollup of {head}")),
        }
    }
    let has_next = contexts
        .pointer("/pageInfo/hasNextPage")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("current-head check rollup of {head} has no pageInfo"))?;
    if !has_next {
        return Ok(None);
    }
    contexts
        .pointer("/pageInfo/endCursor")
        .and_then(Value::as_str)
        .filter(|cursor| !cursor.is_empty())
        .map(|cursor| Some(cursor.to_owned()))
        .ok_or_else(|| format!("current-head check rollup of {head} has no endCursor"))
}

/// REST status histories read on earlier passes, by head, kept beside the
/// steward ledger. Every entry is shown current against the live rollup
/// before it is used ([`HeadRollup::history_is_current`]), so a stale, foreign
/// or damaged file costs a REST read, never a wrong answer.
#[derive(Debug, Default)]
pub(super) struct StatusHistory {
    path: Option<PathBuf>,
    repo: String,
    heads: BTreeMap<String, Vec<Value>>,
}

const HISTORY_SCHEMA: u64 = 1;

/// The fields `parse_rest_status` and [`latest_status_keys`] read; the rest
/// of a status row is not kept.
fn trim_row(row: &Value) -> Value {
    let mut kept = serde_json::Map::new();
    for field in [
        "id",
        "context",
        "state",
        "target_url",
        "created_at",
        "updated_at",
    ] {
        if let Some(value) = row.get(field) {
            kept.insert(field.to_owned(), value.clone());
        }
    }
    if let Some(creator) = row.get("creator").filter(|creator| !creator.is_null()) {
        let mut trimmed = serde_json::Map::new();
        for field in ["login", "id"] {
            if let Some(value) = creator.get(field) {
                trimmed.insert(field.to_owned(), value.clone());
            }
        }
        kept.insert("creator".to_owned(), Value::Object(trimmed));
    } else if row.get("creator").is_some() {
        kept.insert("creator".to_owned(), Value::Null);
    }
    Value::Object(kept)
}

impl StatusHistory {
    /// The store at `path` for `repo`. A missing, unreadable or foreign file
    /// is an empty store.
    pub(super) fn load(path: &Path, repo: &str) -> Self {
        let heads = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| {
                value.get("schema_version").and_then(Value::as_u64) == Some(HISTORY_SCHEMA)
                    && value
                        .get("repo")
                        .and_then(Value::as_str)
                        .is_some_and(|stored| stored.eq_ignore_ascii_case(repo))
            })
            .and_then(|value| value.get("heads").and_then(Value::as_object).cloned())
            .map(|heads| {
                heads
                    .into_iter()
                    .filter_map(|(head, rows)| Some((head, rows.as_array()?.clone())))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            path: Some(path.to_owned()),
            repo: repo.to_owned(),
            heads,
        }
    }

    /// The history held for `head`, if any.
    pub(super) fn get(&self, head: &str) -> Option<&[Value]> {
        self.heads.get(head).map(Vec::as_slice)
    }

    /// Keep exactly `heads` (the histories this pass used) and write them.
    /// Best effort: a failed write only costs the next pass a REST read.
    pub(super) fn replace_and_save(&mut self, heads: BTreeMap<String, Vec<Value>>) {
        self.heads = heads
            .into_iter()
            .map(|(head, rows)| (head, rows.iter().map(trim_row).collect()))
            .collect();
        let Some(path) = self.path.as_deref() else {
            return;
        };
        let payload = serde_json::json!({
            "schema_version": HISTORY_SCHEMA,
            "repo": self.repo,
            "heads": self.heads,
        });
        let Ok(bytes) = serde_json::to_vec(&payload) else {
            return;
        };
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            && std::fs::create_dir_all(parent).is_err()
        {
            return;
        }
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        if std::fs::write(&temp, bytes).is_err() || std::fs::rename(&temp, path).is_err() {
            let _ = std::fs::remove_file(&temp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rest_row(id: u64, context: &str, state: &str, at: &str) -> Value {
        serde_json::json!({
            "id": id, "context": context, "state": state, "target_url": null,
            "created_at": at, "updated_at": at,
            "creator": {"login": "github-actions[bot]", "id": 41_898_282},
            "description": "dropped when stored"
        })
    }

    fn rollup_status(context: &str, state: &str, at: &str) -> Value {
        serde_json::json!({
            "__typename": "StatusContext", "context": context, "state": state,
            "targetUrl": null, "createdAt": at,
            "creator": {"__typename": "Bot", "login": "github-actions", "databaseId": 41_898_282}
        })
    }

    fn rollup_of(statuses: &[Value]) -> HeadRollup {
        let mut rollup = HeadRollup::default();
        for status in statuses {
            rollup.has_statuses = true;
            match rollup_status_key(status) {
                Some(key) => {
                    rollup.latest_statuses.insert(key);
                }
                None => rollup.unkeyable_status = true,
            }
        }
        rollup
    }

    #[test]
    fn a_history_is_current_only_while_every_newest_status_matches_the_rollup() {
        let history = vec![
            rest_row(2, "review-verdict", "success", "2026-11-08T02:00:00Z"),
            rest_row(1, "review-verdict", "pending", "2026-11-08T01:00:00Z"),
            rest_row(3, "freeze", "success", "2026-11-08T03:00:00Z"),
        ];
        let current = rollup_of(&[
            rollup_status("review-verdict", "SUCCESS", "2026-11-08T02:00:00Z"),
            rollup_status("freeze", "SUCCESS", "2026-11-08T03:00:00Z"),
        ]);
        assert!(current.history_is_current(&history));
        // A newer status of a known context, a new context, and a context the
        // rollup no longer names each make the history stale.
        for stale in [
            rollup_of(&[
                rollup_status("review-verdict", "FAILURE", "2026-11-08T04:00:00Z"),
                rollup_status("freeze", "SUCCESS", "2026-11-08T03:00:00Z"),
            ]),
            rollup_of(&[
                rollup_status("review-verdict", "SUCCESS", "2026-11-08T02:00:00Z"),
                rollup_status("freeze", "SUCCESS", "2026-11-08T03:00:00Z"),
                rollup_status("other", "SUCCESS", "2026-11-08T04:00:00Z"),
            ]),
            rollup_of(&[rollup_status(
                "review-verdict",
                "SUCCESS",
                "2026-11-08T02:00:00Z",
            )]),
        ] {
            assert!(!stale.history_is_current(&history));
        }
    }

    #[test]
    fn an_unkeyable_rollup_status_never_validates_a_history() {
        let history = vec![rest_row(1, "x", "success", "2026-11-08T01:00:00Z")];
        let mut expected = rollup_status("x", "SUCCESS", "2026-11-08T01:00:00Z");
        assert!(rollup_of(std::slice::from_ref(&expected)).history_is_current(&history));
        expected["state"] = Value::from("EXPECTED");
        assert!(!rollup_of(std::slice::from_ref(&expected)).history_is_current(&history));
        let mut organization = rollup_status("x", "SUCCESS", "2026-11-08T01:00:00Z");
        organization["creator"]["__typename"] = Value::from("Organization");
        assert!(!rollup_of(&[organization]).history_is_current(&history));
    }

    #[test]
    fn a_rollup_check_run_parses_like_its_rest_row() {
        let rollup = serde_json::json!({
            "__typename": "CheckRun", "name": "macos", "status": "COMPLETED",
            "conclusion": "FAILURE", "databaseId": 9,
            "detailsUrl": "https://github.com/o/r/actions/runs/5/job/9",
            "startedAt": "2026-11-08T01:00:00Z", "completedAt": "2026-11-08T02:00:00Z",
            "checkSuite": {"app": {"databaseId": 15_368}}
        });
        let rest = serde_json::json!({
            "id": 9, "name": "macos", "status": "completed", "conclusion": "failure",
            "details_url": "https://github.com/o/r/actions/runs/5/job/9",
            "started_at": "2026-11-08T01:00:00Z", "completed_at": "2026-11-08T02:00:00Z",
            "app": {"id": 15_368}
        });
        assert_eq!(
            rollup_check_run(&rollup),
            super::super::observation::parse_rest_check(&rest)
        );
    }

    #[test]
    fn the_store_keeps_only_this_passes_heads_and_rejects_another_repository() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("history.json");
        let mut store = StatusHistory::load(&path, "o/r");
        assert!(store.get("a").is_none());
        let row = rest_row(1, "x", "success", "2026-11-08T01:00:00Z");
        store.replace_and_save(BTreeMap::from([("a".to_owned(), vec![row.clone()])]));
        let reloaded = StatusHistory::load(&path, "o/r");
        let kept = reloaded.get("a").expect("kept");
        assert!(
            kept[0].get("description").is_none(),
            "unread fields are dropped"
        );
        assert_eq!(latest_status_keys(kept), latest_status_keys(&[row]));
        assert!(StatusHistory::load(&path, "o/other").get("a").is_none());
        std::fs::write(&path, b"not json").expect("damage");
        assert!(StatusHistory::load(&path, "o/r").get("a").is_none());
    }
}
