//! `shipyard who --pr N`: which host and agent session landed, armed or
//! removed a pull request.
//!
//! GitHub attributes every queue mutation to the same App actor, whoever
//! issued it. The guards record what they judge: `queue-arm-guard` appends
//! one line per arm, enqueue or merge to `queue-arms.jsonl` (time, host,
//! session, agent id when set, repository, pull request, head, verb,
//! verdict), and
//! `queue-removal-guard` appends each overridden removal to
//! `queue-removals.jsonl`. This module joins those ledgers, read from every
//! fleet host, to the pull request's timeline.
//!
//! The join key is repository + pull request + verb + head; time only breaks
//! ties between entries that agree on the key. An event no ledger entry
//! explains is reported `unattributed`, never guessed: a landing made with
//! native `gh` rather than `ghapp` passes no guard and leaves no entry.

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::Value;

/// A ledger entry may be written this long after GitHub records the event
/// (clock skew between hosts and GitHub).
const AFTER_SKEW_SECONDS: i64 = 120;
/// A ledger entry this long before the event is not its cause.
const BEFORE_WINDOW_SECONDS: i64 = 3600;

/// One line from a host's ledger.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LedgerEntry {
    /// Host the line was read from (the class name when the line has none).
    pub host: String,
    /// Agent session, or `unattributed`. Every agent of one Claude Code team
    /// shares it.
    pub session: String,
    /// The per-agent id (`SHIPYARD_AGENT_ID`), when the arming process had one.
    pub agent: Option<String>,
    /// When the guard judged it.
    pub at: DateTime<Utc>,
    /// `OWNER/REPO`, when recorded.
    pub repo: Option<String>,
    /// Pull request, when recorded. A Shipyard-internal GraphQL landing names
    /// only a node id, so its entry is matched by repository and head.
    pub pr: Option<u64>,
    /// Head the request landed, when recorded.
    pub head: Option<String>,
    /// `arm`, `merge` or `remove`.
    pub verb: String,
    /// The guard's verdict; only `allowed` entries can explain an event.
    pub verdict: String,
}

/// Parse one host's `queue-arms.jsonl` and `queue-removals.jsonl` text.
/// Malformed lines are skipped and counted.
#[must_use]
pub fn parse_ledgers(host: &str, arms: &str, removals: &str) -> (Vec<LedgerEntry>, usize) {
    let mut entries = Vec::new();
    let mut skipped = 0;
    for line in arms.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Value>(line)
            .ok()
            .and_then(|value| arm_entry(host, &value))
        {
            Some(entry) => entries.push(entry),
            None => skipped += 1,
        }
    }
    for line in removals.lines().filter(|line| !line.trim().is_empty()) {
        match serde_json::from_str::<Value>(line) {
            Ok(value) => {
                let found = removal_entries(host, &value);
                if found.is_empty() {
                    skipped += 1;
                }
                entries.extend(found);
            }
            Err(_) => skipped += 1,
        }
    }
    (entries, skipped)
}

fn time(value: &Value) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value.as_str()?)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|text| !text.is_empty())
}

fn arm_entry(host: &str, value: &Value) -> Option<LedgerEntry> {
    Some(LedgerEntry {
        host: text(value, "host").unwrap_or_else(|| host.to_owned()),
        session: text(value, "session").unwrap_or_else(|| "unattributed".to_owned()),
        agent: text(value, "agent"),
        at: time(value.get("at")?)?,
        repo: text(value, "repo"),
        pr: value.get("pr").and_then(Value::as_u64),
        head: text(value, "head").map(|head| head.to_ascii_lowercase()),
        verb: text(value, "verb")?,
        verdict: text(value, "verdict")?,
    })
}

fn removal_entries(host: &str, value: &Value) -> Vec<LedgerEntry> {
    let Some(at) = value.get("at").and_then(time) else {
        return Vec::new();
    };
    value
        .get("targets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|target| {
            Some(LedgerEntry {
                host: text(value, "host").unwrap_or_else(|| host.to_owned()),
                session: text(value, "session").unwrap_or_else(|| "unattributed".to_owned()),
                agent: text(value, "agent"),
                at,
                repo: Some(text(target, "repo")?),
                pr: Some(target.get("pr")?.as_u64()?),
                head: text(target, "head").map(|head| head.to_ascii_lowercase()),
                verb: "remove".to_owned(),
                verdict: "allowed".to_owned(),
            })
        })
        .collect()
}

/// A queue or merge event on the pull request's timeline.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TimelineEvent {
    /// GitHub's event type.
    pub kind: String,
    /// `arm`, `merge` or `remove`.
    pub verb: String,
    /// When GitHub recorded it.
    pub at: DateTime<Utc>,
    /// GitHub's actor login (for the App, always the App).
    pub actor: Option<String>,
    /// The pull request's head at that moment, when the timeline shows it.
    pub head: Option<String>,
}

/// The pull-request timeline query `who` sends.
pub const TIMELINE_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){\
repository(owner:$owner,name:$name){pullRequest(number:$number){number headRefOid merged \
timelineItems(last:100,itemTypes:[PULL_REQUEST_COMMIT,HEAD_REF_FORCE_PUSHED_EVENT,\
AUTO_MERGE_ENABLED_EVENT,ADDED_TO_MERGE_QUEUE_EVENT,REMOVED_FROM_MERGE_QUEUE_EVENT,MERGED_EVENT])\
{nodes{__typename ... on PullRequestCommit{commit{oid committedDate}} \
... on HeadRefForcePushedEvent{createdAt afterCommit{oid}} \
... on AutoMergeEnabledEvent{createdAt actor{login}} \
... on AddedToMergeQueueEvent{createdAt actor{login}} \
... on RemovedFromMergeQueueEvent{createdAt reason actor{login} beforeCommit{oid}} \
... on MergedEvent{createdAt actor{login}}}}}}}";

/// The queue and merge events of a [`TIMELINE_QUERY`] answer, oldest first,
/// each with the head the pull request had at that moment.
///
/// # Errors
///
/// When the answer holds no pull request.
pub fn timeline_events(response: &Value) -> Result<Vec<TimelineEvent>, String> {
    let pull = response
        .pointer("/data/repository/pullRequest")
        .filter(|pull| pull.is_object())
        .ok_or_else(|| "the timeline answer holds no pull request".to_owned())?;
    let nodes = pull
        .pointer("/timelineItems/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "the timeline answer lists no events".to_owned())?;
    let final_head = text(pull, "headRefOid").map(|head| head.to_ascii_lowercase());
    // Heads by the time they became the head.
    let mut heads: Vec<(DateTime<Utc>, String)> = Vec::new();
    for node in nodes {
        match node.get("__typename").and_then(Value::as_str) {
            Some("PullRequestCommit") => {
                if let (Some(oid), Some(at)) = (
                    node.pointer("/commit/oid").and_then(Value::as_str),
                    node.pointer("/commit/committedDate").and_then(time),
                ) {
                    heads.push((at, oid.to_ascii_lowercase()));
                }
            }
            Some("HeadRefForcePushedEvent") => {
                if let (Some(oid), Some(at)) = (
                    node.pointer("/afterCommit/oid").and_then(Value::as_str),
                    node.get("createdAt").and_then(time),
                ) {
                    heads.push((at, oid.to_ascii_lowercase()));
                }
            }
            _ => {}
        }
    }
    heads.sort();
    let head_at = |at: DateTime<Utc>| {
        heads
            .iter()
            .rev()
            .find(|(since, _)| *since <= at)
            .map(|(_, head)| head.clone())
    };
    let mut events = Vec::new();
    for node in nodes {
        let kind = node.get("__typename").and_then(Value::as_str).unwrap_or("");
        let verb = match kind {
            "AutoMergeEnabledEvent" | "AddedToMergeQueueEvent" => "arm",
            "MergedEvent" => "merge",
            "RemovedFromMergeQueueEvent" => "remove",
            _ => continue,
        };
        let Some(at) = node.get("createdAt").and_then(time) else {
            continue;
        };
        let head = match kind {
            "RemovedFromMergeQueueEvent" => node
                .pointer("/beforeCommit/oid")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase)
                .or_else(|| head_at(at)),
            "MergedEvent" => final_head.clone(),
            _ => head_at(at).or_else(|| final_head.clone()),
        };
        events.push(TimelineEvent {
            kind: kind.to_owned(),
            verb: verb.to_owned(),
            at,
            actor: node
                .pointer("/actor/login")
                .and_then(Value::as_str)
                .map(str::to_owned),
            head,
        });
    }
    events.sort_by_key(|event| event.at);
    Ok(events)
}

/// One timeline event and who caused it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attribution {
    /// The event.
    pub event: TimelineEvent,
    /// The ledger entry that explains it, when one does.
    pub entry: Option<LedgerEntry>,
    /// Whether the entry's head equals the event's (`None` when either is unknown).
    pub head_match: Option<bool>,
}

impl Attribution {
    /// `host/session`, with ` agent <id>` when the ledger recorded one, or
    /// `unattributed`. Without an agent id this names the host and the team
    /// session, not which agent within it.
    #[must_use]
    pub fn who(&self) -> String {
        self.entry.as_ref().map_or_else(
            || "unattributed".to_owned(),
            |entry| match &entry.agent {
                Some(agent) => format!("{}/{} agent {agent}", entry.host, entry.session),
                None => format!("{}/{}", entry.host, entry.session),
            },
        )
    }
}

/// Whether `entry` is about `repo#pr`: by number, or, when the entry carries
/// no number (a Shipyard-internal GraphQL landing), by the exact head the
/// event landed. A recorded repository must agree either way.
fn names_this_pull_request(entry: &LedgerEntry, repo: &str, pr: u64, event: &TimelineEvent) -> bool {
    if entry
        .repo
        .as_deref()
        .is_some_and(|recorded| !recorded.eq_ignore_ascii_case(repo))
    {
        return false;
    }
    match entry.pr {
        Some(number) => number == pr && entry.repo.is_some(),
        None => matches!(
            (&entry.head, &event.head),
            (Some(left), Some(right)) if left.eq_ignore_ascii_case(right)
        ),
    }
}

/// Join `events` of `repo#pr` to `entries`.
///
/// An entry explains an event when the repository, pull request and verb
/// agree (an entry with no pull request number, which Shipyard's own GraphQL
/// landings write, must name the event's exact head instead), the guard
/// allowed it, and it was written from an hour before the event to two
/// minutes after. Among those, an entry whose head equals the
/// event's head wins; time breaks the remaining ties (the latest entry not
/// after the event, else the earliest after it).
#[must_use]
pub fn attribute(
    repo: &str,
    pr: u64,
    events: &[TimelineEvent],
    entries: &[LedgerEntry],
) -> Vec<Attribution> {
    events
        .iter()
        .map(|event| {
            let candidates: Vec<&LedgerEntry> = entries
                .iter()
                .filter(|entry| {
                    names_this_pull_request(entry, repo, pr, event)
                        && entry.verb == event.verb
                        && entry.verdict == "allowed"
                        && entry.at <= event.at + Duration::seconds(AFTER_SKEW_SECONDS)
                        && entry.at >= event.at - Duration::seconds(BEFORE_WINDOW_SECONDS)
                })
                .collect();
            let same_head = |entry: &&LedgerEntry| match (&entry.head, &event.head) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => false,
            };
            let pool: Vec<&LedgerEntry> = if candidates.iter().any(same_head) {
                candidates.iter().copied().filter(same_head).collect()
            } else {
                candidates
            };
            let distance = |entry: &&LedgerEntry| {
                let delta = (event.at - entry.at).num_seconds();
                // Entries written before the event first, nearest first.
                if delta >= 0 { (0, delta) } else { (1, -delta) }
            };
            let entry = pool.into_iter().min_by_key(distance).cloned();
            let head_match = entry.as_ref().and_then(|entry| {
                Some(
                    entry
                        .head
                        .as_ref()?
                        .eq_ignore_ascii_case(event.head.as_ref()?),
                )
            });
            Attribution {
                event: event.clone(),
                entry,
                head_match,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
