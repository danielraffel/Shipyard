//! Stable, read-only GitHub merge-queue snapshots and delta tracking.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Persisted schema version for queue-observer state and transition records.
pub const QUEUE_OBSERVER_SCHEMA_VERSION: u32 = 1;

/// Adaptive polling intervals, in seconds. A transition resets to the first
/// value; every unchanged observation advances one step and then stays capped.
pub const BACKOFF_SECONDS: [u64; 5] = [15, 30, 60, 120, 300];

/// One required-check observation, collapsed to the latest context instance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckSnapshot {
    /// Check or status-context name.
    pub name: String,
    /// GitHub lifecycle state, normalized to lowercase.
    pub status: String,
    /// Terminal conclusion, normalized to lowercase when present.
    pub conclusion: Option<String>,
    /// Whether repository governance names this context as required.
    pub required: bool,
    /// Details URL supplied by GitHub.
    pub url: Option<String>,
}

/// One open pull request targeting the observed base branch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PullRequestSnapshot {
    /// Pull request number.
    pub number: u64,
    /// Browser URL.
    pub url: String,
    /// Exact pull request head SHA.
    pub head_sha: String,
    /// GitHub merge-state classification.
    pub merge_state: String,
    /// Whether an auto-merge request is active.
    pub auto_merge: bool,
    /// Human owners, derived from `shipyard:owner/<name>` labels or assignees.
    pub owners: Vec<String>,
    /// Explicit blockers from `shipyard:blocker/<reason>` labels.
    pub blockers: Vec<String>,
    /// Latest check/status observation for each context.
    pub checks: Vec<CheckSnapshot>,
}

/// One server-owned merge-queue entry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueueEntrySnapshot {
    /// Pull request number.
    pub pr: u64,
    /// Queue position as reported by GitHub.
    pub position: u64,
    /// Pull request URL.
    pub url: String,
    /// Current pull request head SHA.
    pub pr_head_sha: String,
    /// Speculative merge-group SHA, when GitHub has materialized one.
    pub merge_group_sha: Option<String>,
    /// Queue admission time.
    pub enqueued_at: String,
    /// Latest check/status observations on the speculative merge-group SHA.
    pub checks: Vec<CheckSnapshot>,
}

/// Local mutation-authority facts. They are observed only; this module exposes
/// no operation capable of changing the hold or authority configuration.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct OwnershipSnapshot {
    /// Machine currently running the collector.
    pub machine: Option<String>,
    /// Configured mutation-authority machine.
    pub mutation_machine: Option<String>,
    /// Whether the current machine is the configured authority.
    pub authority_matches: bool,
    /// Durable hold reason, when queue mutation is paused.
    pub hold_reason: Option<String>,
    /// Machine that created the durable hold.
    pub hold_machine: Option<String>,
    /// Time at which the durable hold was created.
    pub held_at: Option<String>,
    /// Read failure that prevents the ownership boundary from being trusted.
    pub blocker: Option<String>,
}

/// Canonical repository queue state. Ordered fields and sorted collections make
/// its serialized bytes and SHA-256 hash stable across equivalent API payloads.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct QueueStateSnapshot {
    /// Schema version.
    pub schema_version: u32,
    /// `owner/name` repository slug.
    pub repo: String,
    /// Monitored base branch.
    pub base: String,
    /// Exact base-branch SHA.
    pub main_sha: String,
    /// Browser URL for the exact base commit.
    pub main_url: String,
    /// Whether any bounded GraphQL connection was truncated.
    pub truncated: bool,
    /// Required check names from repository/configured governance.
    pub required_contexts: Vec<String>,
    /// Local mutation ownership and hold state.
    pub ownership: OwnershipSnapshot,
    /// Queue entries in queue order.
    pub queue: Vec<QueueEntrySnapshot>,
    /// Open pull requests ordered by number.
    pub pull_requests: Vec<PullRequestSnapshot>,
}

/// Persisted observer cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObserverState {
    /// Schema version.
    pub schema_version: u32,
    /// SHA-256 of canonical snapshot JSON.
    pub state_hash: String,
    /// Last canonical snapshot.
    pub snapshot: QueueStateSnapshot,
    /// Index into [`BACKOFF_SECONDS`] used for the next poll.
    pub backoff_index: usize,
}

/// One semantic change between canonical snapshots.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StateChange {
    /// Stable machine-readable path.
    pub path: String,
    /// Previous value, absent for an initial observation or addition.
    pub before: Option<Value>,
    /// Current value, absent for a removal.
    pub after: Option<Value>,
}

/// Emitted only for the initial observation or a real state transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Transition {
    /// Schema version.
    pub schema_version: u32,
    /// `initial` or `delta`.
    pub kind: String,
    /// Canonical snapshot hash.
    pub state_hash: String,
    /// Delay before the next live poll.
    pub next_poll_seconds: u64,
    /// Semantic changes from the previous snapshot.
    pub changes: Vec<StateChange>,
    /// Full current snapshot, so a new consumer needs no chat history.
    pub snapshot: QueueStateSnapshot,
}

/// Result of applying one observation to persisted state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObservationResult {
    /// Updated durable state.
    pub state: ObserverState,
    /// Transition to emit, or `None` when the state is unchanged.
    pub transition: Option<Transition>,
}

/// Compute a stable SHA-256 over canonical snapshot JSON.
pub fn snapshot_hash(snapshot: &QueueStateSnapshot) -> Result<String, serde_json::Error> {
    serde_json::to_vec(snapshot).map(|bytes| format!("{:x}", Sha256::digest(bytes)))
}

/// Apply one canonical snapshot to the previous observer cursor.
pub fn observe(
    previous: Option<&ObserverState>,
    snapshot: QueueStateSnapshot,
) -> Result<ObservationResult, serde_json::Error> {
    let state_hash = snapshot_hash(&snapshot)?;
    if previous.is_some_and(|state| state.state_hash == state_hash) {
        let previous = previous.expect("checked above");
        let backoff_index = (previous.backoff_index + 1).min(BACKOFF_SECONDS.len() - 1);
        return Ok(ObservationResult {
            state: ObserverState {
                schema_version: QUEUE_OBSERVER_SCHEMA_VERSION,
                state_hash,
                snapshot,
                backoff_index,
            },
            transition: None,
        });
    }

    let changes = previous.map_or_else(
        || {
            vec![StateChange {
                path: "/".to_owned(),
                before: None,
                after: serde_json::to_value(&snapshot).ok(),
            }]
        },
        |state| diff_snapshots(&state.snapshot, &snapshot),
    );
    let state = ObserverState {
        schema_version: QUEUE_OBSERVER_SCHEMA_VERSION,
        state_hash: state_hash.clone(),
        snapshot: snapshot.clone(),
        backoff_index: 0,
    };
    Ok(ObservationResult {
        transition: Some(Transition {
            schema_version: QUEUE_OBSERVER_SCHEMA_VERSION,
            kind: if previous.is_some() {
                "delta"
            } else {
                "initial"
            }
            .to_owned(),
            state_hash,
            next_poll_seconds: BACKOFF_SECONDS[0],
            changes,
            snapshot,
        }),
        state,
    })
}

/// Delay associated with a durable observer cursor.
#[must_use]
pub fn next_poll_seconds(state: &ObserverState) -> u64 {
    BACKOFF_SECONDS[state.backoff_index.min(BACKOFF_SECONDS.len() - 1)]
}

/// Number of adaptive queries needed to cover an unchanged interval. This is
/// deliberately public so efficiency receipts can be generated without
/// running a wall-clock benchmark.
#[must_use]
pub fn adaptive_query_count(duration_seconds: u64) -> u64 {
    let mut elapsed = 0;
    let mut index = 0;
    let mut count = 0;
    while elapsed < duration_seconds {
        count += 1;
        elapsed += BACKOFF_SECONDS[index];
        index = (index + 1).min(BACKOFF_SECONDS.len() - 1);
    }
    count
}

/// Load a prior cursor. A missing file is a clean first run.
pub fn load_state(path: &Path) -> Result<Option<ObserverState>, String> {
    match fs::read_to_string(path) {
        Ok(raw) => {
            let state: ObserverState = serde_json::from_str(&raw)
                .map_err(|error| format!("parse observer state {}: {error}", path.display()))?;
            if state.schema_version != QUEUE_OBSERVER_SCHEMA_VERSION {
                return Err(format!(
                    "observer state {} has unsupported schema version {}",
                    path.display(),
                    state.schema_version
                ));
            }
            let actual = snapshot_hash(&state.snapshot)
                .map_err(|error| format!("hash observer state {}: {error}", path.display()))?;
            if actual != state.state_hash {
                return Err(format!(
                    "observer state {} hash mismatch: stored={} actual={actual}",
                    path.display(),
                    state.state_hash
                ));
            }
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("read observer state {}: {error}", path.display())),
    }
}

/// Atomically persist the latest cursor.
pub fn save_state(path: &Path, state: &ObserverState) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("observer state path {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create observer state directory: {error}"))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| format!("create observer state temporary file: {error}"))?;
    serde_json::to_writer_pretty(&mut temp, state)
        .map_err(|error| format!("encode observer state: {error}"))?;
    temp.write_all(b"\n")
        .map_err(|error| format!("finish observer state: {error}"))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| format!("sync observer state: {error}"))?;
    temp.persist(path)
        .map_err(|error| format!("persist observer state: {}", error.error))?;
    Ok(())
}

/// Append one transition as compact NDJSON.
pub fn append_transition(path: &Path, transition: &Transition) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("transition log path {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create transition log directory: {error}"))?;
    let lock_path = path.with_extension("append.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .map_err(|error| format!("open transition-log lock {}: {error}", lock_path.display()))?;
    lock.lock_exclusive().map_err(|error| {
        format!(
            "acquire transition-log lock {}: {error}",
            lock_path.display()
        )
    })?;
    let mut payload = serde_json::to_vec(transition)
        .map_err(|error| format!("encode transition log: {error}"))?;
    payload.push(b'\n');
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open transition log {}: {error}", path.display()))?;
    repair_incomplete_transition_tail(path, &mut file)?;
    file.write_all(&payload)
        .map_err(|error| format!("append transition log: {error}"))?;
    file.sync_data()
        .map_err(|error| format!("sync transition log: {error}"))?;
    Ok(())
}

fn repair_incomplete_transition_tail(path: &Path, file: &mut fs::File) -> Result<(), String> {
    let len = file
        .metadata()
        .map_err(|error| format!("inspect transition log {}: {error}", path.display()))?
        .len();
    if len == 0 {
        return Ok(());
    }
    file.seek(SeekFrom::End(-1))
        .and_then(|_| {
            let mut last = [0_u8; 1];
            file.read_exact(&mut last).map(|()| last)
        })
        .map_err(|error| format!("inspect transition log tail {}: {error}", path.display()))
        .and_then(|last| {
            if last == *b"\n" {
                Ok(())
            } else {
                truncate_to_last_complete_line(path, file, len)
            }
        })
}

fn truncate_to_last_complete_line(
    path: &Path,
    file: &mut fs::File,
    mut end: u64,
) -> Result<(), String> {
    let mut buffer = [0_u8; 8192];
    let complete_len = loop {
        if end == 0 {
            break 0;
        }
        let chunk_len = usize::try_from(end.min(buffer.len() as u64)).expect("bounded chunk");
        let start = end - chunk_len as u64;
        file.seek(SeekFrom::Start(start))
            .and_then(|_| file.read_exact(&mut buffer[..chunk_len]))
            .map_err(|error| format!("scan transition log {}: {error}", path.display()))?;
        if let Some(index) = buffer[..chunk_len].iter().rposition(|byte| *byte == b'\n') {
            break start + index as u64 + 1;
        }
        end = start;
    };
    file.set_len(complete_len)
        .map_err(|error| format!("repair transition log {}: {error}", path.display()))
}

/// Default state and transition-log paths for a repository/base pair.
#[must_use]
pub fn default_paths(state_root: &Path, repo: &str, base: &str) -> (PathBuf, PathBuf) {
    let digest = format!("{:x}", Sha256::digest(format!("{repo}\0{base}").as_bytes()));
    let stem = format!("{}-{}", repo.replace('/', "-"), &digest[..24]);
    let dir = state_root.join("queue-observer");
    (
        dir.join(format!("{stem}.json")),
        dir.join(format!("{stem}.transitions.jsonl")),
    )
}

/// Parse one compact GraphQL response into canonical state.
pub fn parse_snapshot(
    body: &Value,
    repo: &str,
    base: &str,
    configured_required: &[String],
    ownership: OwnershipSnapshot,
) -> Result<QueueStateSnapshot, String> {
    if let Some(errors) = body
        .get("errors")
        .and_then(Value::as_array)
        .filter(|errors| !errors.is_empty())
    {
        return Err(format!(
            "queue snapshot GraphQL errors: {}",
            Value::Array(errors.clone())
        ));
    }
    let repository = body
        .pointer("/data/repository")
        .filter(|value| !value.is_null())
        .ok_or_else(|| "queue snapshot response missing repository".to_owned())?;
    let main_sha = repository
        .pointer("/baseRef/target/oid")
        .and_then(Value::as_str)
        .filter(|sha| !sha.is_empty())
        .ok_or_else(|| format!("queue snapshot response missing refs/heads/{base}"))?
        .to_owned();
    let repository_url = repository
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("https://github.com");

    let mut required = configured_required.iter().cloned().collect::<BTreeSet<_>>();
    if let Some(rule) = repository.pointer("/baseRef/branchProtectionRule") {
        required.extend(
            rule.get("requiredStatusCheckContexts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_owned),
        );
    }

    let mut pull_requests = connection_nodes(repository.get("pullRequests"))
        .into_iter()
        .map(|node| parse_pull_request(node, &required))
        .collect::<Result<Vec<_>, _>>()?;
    pull_requests.sort_by_key(|pr| pr.number);

    let queue_value = repository
        .get("mergeQueue")
        .filter(|value| !value.is_null());
    let mut queue = queue_value
        .map(|value| connection_nodes(value.get("entries")))
        .unwrap_or_default()
        .into_iter()
        .map(|node| parse_queue_entry(node, &required))
        .collect::<Result<Vec<_>, _>>()?;
    queue.sort_by_key(|entry| (entry.position, entry.pr));

    let truncated = connection_has_next(repository.get("pullRequests"))
        || queue_value.is_some_and(|value| connection_has_next(value.get("entries")))
        || queue_value.is_some_and(|value| {
            connection_nodes(value.get("entries")).iter().any(|entry| {
                connection_has_next(entry.pointer("/headCommit/statusCheckRollup/contexts"))
            })
        })
        || connection_nodes(repository.get("pullRequests"))
            .iter()
            .any(|pr| {
                connection_has_next(pr.pointer("/statusCheckRollup/contexts"))
                    || connection_has_next(pr.get("assignees"))
                    || connection_has_next(pr.get("labels"))
            });

    Ok(QueueStateSnapshot {
        schema_version: QUEUE_OBSERVER_SCHEMA_VERSION,
        repo: repo.to_owned(),
        base: base.to_owned(),
        main_url: format!("{repository_url}/commit/{main_sha}"),
        main_sha,
        truncated,
        required_contexts: required.into_iter().collect(),
        ownership,
        queue,
        pull_requests,
    })
}

/// Render a transition as compact Markdown while retaining every repository,
/// pull-request, check, and commit URL present in the snapshot.
#[must_use]
pub fn render_markdown(transition: &Transition) -> String {
    let snapshot = &transition.snapshot;
    let mut lines = vec![
        format!("## Queue {}", transition.kind),
        format!(
            "- `{}` `{}`: [{} `{}`]({}); hash `{}`; next poll {}s{}",
            snapshot.repo,
            snapshot.base,
            snapshot.base,
            snapshot.main_sha,
            snapshot.main_url,
            transition.state_hash,
            transition.next_poll_seconds,
            if snapshot.truncated {
                "; **truncated**"
            } else {
                ""
            }
        ),
    ];
    let owner = snapshot
        .ownership
        .mutation_machine
        .as_deref()
        .unwrap_or("unconfigured");
    let mut authority = format!("- queue owner: `{owner}`");
    if let Some(reason) = &snapshot.ownership.hold_reason {
        let _ = write!(authority, "; **HOLD:** {reason}");
    }
    if let Some(blocker) = &snapshot.ownership.blocker {
        let _ = write!(authority, "; **BLOCKER:** {blocker}");
    }
    lines.push(authority);
    if snapshot.queue.is_empty() {
        lines.push("- queue: empty".to_owned());
    } else {
        for entry in &snapshot.queue {
            lines.push(format!(
                "- queue {}: [PR #{}]({}) head `{}`{}; enqueued {}",
                entry.position,
                entry.pr,
                entry.url,
                entry.pr_head_sha,
                entry
                    .merge_group_sha
                    .as_deref()
                    .map_or_else(String::new, |sha| format!("; merge group `{sha}`")),
                entry.enqueued_at
            ));
            for check in &entry.checks {
                lines.push(format!("  - merge-group {}", render_check(check)));
            }
        }
    }
    for pr in &snapshot.pull_requests {
        let owners = if pr.owners.is_empty() {
            "unowned".to_owned()
        } else {
            pr.owners.join(",")
        };
        let blockers = if pr.blockers.is_empty() {
            String::new()
        } else {
            format!("; blockers={}", pr.blockers.join(","))
        };
        lines.push(format!(
            "- [PR #{}]({}): head `{}`; state={}; auto_merge={}; owners={}{}",
            pr.number, pr.url, pr.head_sha, pr.merge_state, pr.auto_merge, owners, blockers
        ));
        for check in &pr.checks {
            lines.push(format!("  - {}", render_check(check)));
        }
    }
    lines.join("\n") + "\n"
}

fn parse_pull_request(
    node: &Value,
    required: &BTreeSet<String>,
) -> Result<PullRequestSnapshot, String> {
    let number = required_u64(node, "number", "pull request")?;
    let labels = connection_nodes(node.get("labels"))
        .into_iter()
        .filter_map(|label| label.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    let mut owners = labels
        .iter()
        .filter_map(|label| label.strip_prefix("shipyard:owner/"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if owners.is_empty() {
        owners.extend(
            connection_nodes(node.get("assignees"))
                .into_iter()
                .filter_map(|assignee| assignee.get("login").and_then(Value::as_str))
                .map(str::to_owned),
        );
    }
    owners.sort();
    owners.dedup();
    let mut blockers = labels
        .iter()
        .filter_map(|label| label.strip_prefix("shipyard:blocker/"))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    blockers.sort();
    blockers.dedup();

    let mut latest = BTreeMap::<String, (String, CheckSnapshot)>::new();
    for check in connection_nodes(node.pointer("/statusCheckRollup/contexts")) {
        let (stamp, parsed) = parse_check(check, required)?;
        match latest.get(&parsed.name) {
            Some((existing, _)) if existing >= &stamp => {}
            _ => {
                latest.insert(parsed.name.clone(), (stamp, parsed));
            }
        }
    }
    let checks = latest.into_values().map(|(_, check)| check).collect();
    Ok(PullRequestSnapshot {
        number,
        url: required_string(node, "url", "pull request")?,
        head_sha: required_string(node, "headRefOid", "pull request")?,
        merge_state: node
            .get("mergeStateStatus")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_ascii_lowercase(),
        auto_merge: node
            .get("autoMergeRequest")
            .is_some_and(|value| !value.is_null()),
        owners,
        blockers,
        checks,
    })
}

fn parse_check(
    node: &Value,
    required: &BTreeSet<String>,
) -> Result<(String, CheckSnapshot), String> {
    let typename = node.get("__typename").and_then(Value::as_str).unwrap_or("");
    let (name, status, conclusion, url, stamp) = match typename {
        "CheckRun" => {
            let name = required_string(node, "name", "check run")?;
            let status = node
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_ascii_lowercase();
            let conclusion = node
                .get("conclusion")
                .and_then(Value::as_str)
                .map(str::to_ascii_lowercase);
            let database_id = node.get("databaseId").and_then(Value::as_u64).unwrap_or(0);
            let observed_at = node
                .pointer("/checkSuite/createdAt")
                .or_else(|| node.get("startedAt"))
                .and_then(Value::as_str);
            // Both context variants use an ISO-8601 timestamp as the primary key.
            // The synthetic high key is only for old replay fixtures; live queries
            // always include the check suite creation time, even for queued runs.
            let stamp = observed_at.map_or_else(
                || format!("~|check:{database_id:020}"),
                |timestamp| format!("{timestamp}|check:{database_id:020}"),
            );
            (
                name,
                status,
                conclusion,
                optional_string(node, "detailsUrl"),
                stamp,
            )
        }
        "StatusContext" => {
            let name = required_string(node, "context", "status context")?;
            let state = node
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_ascii_lowercase();
            let conclusion =
                matches!(state.as_str(), "success" | "failure" | "error").then(|| state.clone());
            let status = if conclusion.is_some() {
                "completed".to_owned()
            } else {
                state
            };
            let stamp = format!(
                "{}|status",
                node.get("createdAt").and_then(Value::as_str).unwrap_or("")
            );
            (
                name,
                status,
                conclusion,
                optional_string(node, "targetUrl"),
                stamp,
            )
        }
        _ => return Err(format!("unsupported status context type `{typename}`")),
    };
    Ok((
        stamp,
        CheckSnapshot {
            required: required.contains(&name),
            name,
            status,
            conclusion,
            url,
        },
    ))
}

fn parse_queue_entry(
    node: &Value,
    required: &BTreeSet<String>,
) -> Result<QueueEntrySnapshot, String> {
    let pr = node
        .get("pullRequest")
        .ok_or_else(|| "queue entry missing pull request".to_owned())?;
    let mut latest = BTreeMap::<String, (String, CheckSnapshot)>::new();
    for check in connection_nodes(node.pointer("/headCommit/statusCheckRollup/contexts")) {
        let (stamp, parsed) = parse_check(check, required)?;
        match latest.get(&parsed.name) {
            Some((existing, _)) if existing >= &stamp => {}
            _ => {
                latest.insert(parsed.name.clone(), (stamp, parsed));
            }
        }
    }
    Ok(QueueEntrySnapshot {
        pr: required_u64(pr, "number", "queue pull request")?,
        position: required_u64(node, "position", "queue entry")?,
        url: required_string(pr, "url", "queue pull request")?,
        pr_head_sha: required_string(pr, "headRefOid", "queue pull request")?,
        merge_group_sha: node
            .pointer("/headCommit/oid")
            .and_then(Value::as_str)
            .map(str::to_owned),
        enqueued_at: required_string(node, "enqueuedAt", "queue entry")?,
        checks: latest.into_values().map(|(_, check)| check).collect(),
    })
}

fn render_check(check: &CheckSnapshot) -> String {
    let value = check.conclusion.as_deref().unwrap_or(&check.status);
    let rendered = check.url.as_deref().map_or_else(
        || format!("`{}`", check.name),
        |url| format!("[{}]({url})", check.name),
    );
    format!(
        "{}{}={value}",
        if check.required { "required " } else { "" },
        rendered
    )
}

fn diff_snapshots(before: &QueueStateSnapshot, after: &QueueStateSnapshot) -> Vec<StateChange> {
    let before = serde_json::to_value(before).expect("snapshot serialization");
    let after = serde_json::to_value(after).expect("snapshot serialization");
    let mut changes = Vec::new();
    diff_values("", &before, &after, &mut changes);
    changes
}

fn diff_values(path: &str, before: &Value, after: &Value, changes: &mut Vec<StateChange>) {
    match (before, after) {
        (Value::Object(left), Value::Object(right)) => {
            let keys = left.keys().chain(right.keys()).collect::<BTreeSet<_>>();
            for key in keys {
                let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                match (left.get(key), right.get(key)) {
                    (Some(left), Some(right)) => diff_values(&child, left, right, changes),
                    (Some(left), None) => changes.push(StateChange {
                        path: child,
                        before: Some(left.clone()),
                        after: None,
                    }),
                    (None, Some(right)) => changes.push(StateChange {
                        path: child,
                        before: None,
                        after: Some(right.clone()),
                    }),
                    (None, None) => unreachable!(),
                }
            }
        }
        _ if before != after => changes.push(StateChange {
            path: path.to_owned(),
            before: Some(before.clone()),
            after: Some(after.clone()),
        }),
        _ => {}
    }
}

fn connection_nodes(value: Option<&Value>) -> Vec<&Value> {
    value
        .and_then(|value| value.get("nodes"))
        .and_then(Value::as_array)
        .map(|nodes| nodes.iter().collect())
        .unwrap_or_default()
}

fn connection_has_next(value: Option<&Value>) -> bool {
    value
        .and_then(|value| value.pointer("/pageInfo/hasNextPage"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn required_string(value: &Value, field: &str, context: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| format!("{context} missing {field}"))
}

fn optional_string(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn required_u64(value: &Value, field: &str, context: &str) -> Result<u64, String> {
    value
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("{context} missing {field}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adaptive_backoff_reduces_one_hour_query_count_by_at_least_ninety_percent() {
        let baseline = 3600 / BACKOFF_SECONDS[0];
        let adaptive = adaptive_query_count(3600);
        assert!(
            adaptive * 10 <= baseline,
            "adaptive={adaptive} baseline={baseline}"
        );
        assert_eq!(adaptive, 16);
    }

    #[test]
    fn unchanged_state_is_silent_and_backoff_resets_on_transition() {
        let snapshot = minimal_snapshot("a");
        let first = observe(None, snapshot.clone()).expect("first");
        assert!(first.transition.is_some());
        assert_eq!(next_poll_seconds(&first.state), 15);
        let same = observe(Some(&first.state), snapshot).expect("same");
        assert!(same.transition.is_none());
        assert_eq!(next_poll_seconds(&same.state), 30);
        let changed = observe(Some(&same.state), minimal_snapshot("b")).expect("changed");
        assert!(changed.transition.is_some());
        assert_eq!(next_poll_seconds(&changed.state), 15);
    }

    #[test]
    fn state_hash_is_independent_of_source_object_order() {
        let left = serde_json::json!({"data":{"repository":fixture_repo("a")}});
        let right: Value =
            serde_json::from_str(&serde_json::to_string(&left).expect("json")).expect("json");
        let left = parse_snapshot(
            &left,
            "o/r",
            "main",
            &["macos".to_owned()],
            OwnershipSnapshot::default(),
        )
        .expect("left");
        let right = parse_snapshot(
            &right,
            "o/r",
            "main",
            &["macos".to_owned()],
            OwnershipSnapshot::default(),
        )
        .expect("right");
        assert_eq!(
            snapshot_hash(&left).expect("hash"),
            snapshot_hash(&right).expect("hash")
        );
    }

    #[test]
    fn persisted_state_detects_tampering() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("state.json");
        let state = observe(None, minimal_snapshot("a")).expect("state").state;
        save_state(&path, &state).expect("save");
        assert_eq!(load_state(&path).expect("load"), Some(state));
        let raw = fs::read_to_string(&path)
            .expect("read")
            .replace("commit/a", "commit/b");
        fs::write(&path, raw).expect("tamper");
        assert!(
            load_state(&path)
                .expect_err("mismatch")
                .contains("hash mismatch")
        );
    }

    #[test]
    fn transition_append_repairs_an_incomplete_tail() {
        let temp = tempfile::tempdir().expect("temp");
        let path = temp.path().join("transitions.jsonl");
        let first = observe(None, minimal_snapshot("a"))
            .expect("first")
            .transition
            .expect("transition");
        append_transition(&path, &first).expect("append first");
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open partial")
            .write_all(b"{\"partial\":")
            .expect("write partial");
        let second = observe(None, minimal_snapshot("b"))
            .expect("second")
            .transition
            .expect("transition");
        append_transition(&path, &second).expect("repair and append");
        let lines = fs::read_to_string(&path).expect("read");
        let records = lines
            .lines()
            .map(serde_json::from_str::<Transition>)
            .collect::<Result<Vec<_>, _>>()
            .expect("valid ndjson");
        assert_eq!(records, [first, second]);
    }

    #[test]
    fn markdown_preserves_sha_urls_owner_and_blocker() {
        let mut snapshot = minimal_snapshot("a");
        snapshot.ownership.mutation_machine = Some("M1".to_owned());
        snapshot.ownership.hold_reason = Some("owned by queue monitor".to_owned());
        snapshot.pull_requests.push(PullRequestSnapshot {
            number: 7,
            url: "https://github.test/o/r/pull/7".to_owned(),
            head_sha: "h".repeat(40),
            merge_state: "blocked".to_owned(),
            auto_merge: false,
            owners: vec!["surface55".to_owned()],
            blockers: vec!["required-checks".to_owned()],
            checks: vec![CheckSnapshot {
                name: "macos".to_owned(),
                status: "completed".to_owned(),
                conclusion: Some("failure".to_owned()),
                required: true,
                url: Some("https://github.test/check/1".to_owned()),
            }],
        });
        let transition = observe(None, snapshot)
            .expect("transition")
            .transition
            .expect("emit");
        let markdown = render_markdown(&transition);
        for expected in [
            "commit/a",
            "M1",
            "owned by queue monitor",
            "pull/7",
            &"h".repeat(40),
            "surface55",
            "required-checks",
            "check/1",
        ] {
            assert!(
                markdown.contains(expected),
                "missing {expected}: {markdown}"
            );
        }
    }

    #[test]
    fn newer_queued_check_run_supersedes_older_completed_run() {
        let required = BTreeSet::from(["macos".to_owned()]);
        let old = serde_json::json!({
            "__typename":"CheckRun", "databaseId":10, "name":"macos",
            "status":"COMPLETED", "conclusion":"SUCCESS",
            "detailsUrl":"https://github.test/run/10", "startedAt":"2026-01-01T00:00:00Z"
        });
        let queued = serde_json::json!({
            "__typename":"CheckRun", "databaseId":11, "name":"macos",
            "status":"QUEUED", "conclusion":null,
            "detailsUrl":"https://github.test/run/11", "startedAt":null
        });
        let mut latest = BTreeMap::<String, (String, CheckSnapshot)>::new();
        for value in [&old, &queued] {
            let (stamp, check) = parse_check(value, &required).expect("check");
            match latest.get(&check.name) {
                Some((existing, _)) if existing >= &stamp => {}
                _ => {
                    latest.insert(check.name.clone(), (stamp, check));
                }
            }
        }
        assert_eq!(latest["macos"].1.status, "queued");
        assert_eq!(
            latest["macos"].1.url.as_deref(),
            Some("https://github.test/run/11")
        );

        let old_status = serde_json::json!({
            "__typename":"StatusContext", "context":"macos", "state":"SUCCESS",
            "targetUrl":"https://github.test/status/9", "createdAt":"2026-01-01T00:00:00Z"
        });
        let new_check = serde_json::json!({
            "__typename":"CheckRun", "databaseId":12, "name":"macos",
            "status":"QUEUED", "conclusion":null, "detailsUrl":"https://github.test/run/12",
            "startedAt":null, "checkSuite":{"createdAt":"2026-01-02T00:00:00Z"}
        });
        let (old_stamp, _) = parse_check(&old_status, &required).expect("status");
        let (new_stamp, _) = parse_check(&new_check, &required).expect("check");
        assert!(new_stamp > old_stamp);
    }

    fn minimal_snapshot(sha: &str) -> QueueStateSnapshot {
        QueueStateSnapshot {
            schema_version: QUEUE_OBSERVER_SCHEMA_VERSION,
            repo: "o/r".to_owned(),
            base: "main".to_owned(),
            main_sha: sha.to_owned(),
            main_url: format!("https://github.test/o/r/commit/{sha}"),
            truncated: false,
            required_contexts: Vec::new(),
            ownership: OwnershipSnapshot::default(),
            queue: Vec::new(),
            pull_requests: Vec::new(),
        }
    }

    fn fixture_repo(sha: &str) -> Value {
        serde_json::json!({
            "url":"https://github.test/o/r",
            "baseRef":{"target":{"oid":sha},"branchProtectionRule":{"requiredStatusCheckContexts":["macos"]}},
            "pullRequests":{"nodes":[],"pageInfo":{"hasNextPage":false}},
            "mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}
        })
    }
}
