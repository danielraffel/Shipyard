//! Pull-request owner registry: which agent session owns a pull request, and
//! how to reach it.
//!
//! `shipyard pr` (and every `shipyard ship` push) records the session that ran
//! it: the host, the agent client, the cmux socket and surface, the agent id
//! and the session id. The record is kept twice:
//!
//! * durably on the registering host, at
//!   `<state_dir>/owners/<owner>/<name>/<pr>.json`, written atomically;
//! * as one HTML comment in the pull request body,
//!   `<!-- shipyard-owner: host=m5 client=codex surface=... session=... -->`,
//!   so the address survives the loss of that host and is readable from any
//!   host with one GraphQL read.
//!
//! The record is an address for a notification, never an authority: nothing
//! here may arm, merge, approve or post a verdict, and a forged marker can at
//! worst send a status line to the wrong terminal. [`crate::relay`] reads it
//! to deliver pull-request events to the owner.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[cfg(test)]
mod tests;

/// Opening text of the pull-request body marker.
pub(crate) const MARKER_PREFIX: &str = "<!-- shipyard-owner:";
/// Closing text of the pull-request body marker.
const MARKER_SUFFIX: &str = "-->";
/// Schema version of an [`OwnerRecord`] on disk.
const RECORD_SCHEMA: u32 = 1;

/// Which agent client a session runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Client {
    /// Claude Code.
    ClaudeCode,
    /// `OpenAI` Codex.
    Codex,
    /// Anything else, including a human shell.
    Other,
}

impl Client {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Other => "other",
        }
    }

    fn parse(text: &str) -> Self {
        match text {
            "claude-code" => Self::ClaudeCode,
            "codex" => Self::Codex,
            _ => Self::Other,
        }
    }
}

/// How a record came to exist.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Source {
    /// `shipyard pr` / `shipyard ship` registered the pushing session.
    Ship,
    /// `shipyard owner register` by hand, or from a pre-push hook.
    Manual,
    /// Read back from the pull-request body marker.
    BodyMarker,
}

/// One pull request's owning session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct OwnerRecord {
    /// On-disk schema.
    #[serde(default = "record_schema")]
    pub(crate) schema: u32,
    /// `OWNER/REPO`.
    pub(crate) repo: String,
    /// Pull request number.
    pub(crate) pr: u64,
    /// Short host name the relay uses to reach the session (`m3`, `m5`).
    pub(crate) host: String,
    /// Agent client.
    pub(crate) client: Client,
    /// cmux socket the session's terminal belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cmux_socket: Option<String>,
    /// cmux surface id (`CMUX_SURFACE_ID`), when the session runs in cmux.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) surface: Option<String>,
    /// Orchestrator-assigned agent id (`SHIPYARD_AGENT_ID`), when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent: Option<String>,
    /// Agent session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session: Option<String>,
    /// When this record was written.
    pub(crate) registered_at: DateTime<Utc>,
    /// How it was written.
    pub(crate) source: Source,
    /// The owner handed the pull request to the repair lane
    /// (`shipyard owner release`).
    #[serde(default)]
    pub(crate) released: bool,
}

fn record_schema() -> u32 {
    RECORD_SCHEMA
}

/// The registering session, read from the environment.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct SessionIdentity {
    pub(crate) host: String,
    pub(crate) client: Option<Client>,
    pub(crate) cmux_socket: Option<String>,
    pub(crate) surface: Option<String>,
    pub(crate) agent: Option<String>,
    pub(crate) session: Option<String>,
}

impl SessionIdentity {
    /// Read this process's session from `env`. `host` is the configured
    /// short host name ([`resolve_host_name`]).
    pub(crate) fn from_env(host: String, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| {
            env(name)
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty())
        };
        // Claude Code first: a Claude session can also carry the Codex
        // companion's id, and is still a Claude session.
        let (client, session) = if let Some(id) = get("CLAUDE_CODE_SESSION_ID") {
            (Some(Client::ClaudeCode), Some(id))
        } else if let Some(id) = get("CODEX_THREAD_ID").or_else(|| get("CODEX_SESSION_ID")) {
            (Some(Client::Codex), Some(id))
        } else if let Some(id) = get("CODEX_COMPANION_SESSION_ID") {
            (Some(Client::Codex), Some(id))
        } else {
            (None, None)
        };
        // A scheduled run exports its own per-run id; it outranks the client's.
        let session = get("SHIPYARD_STAMP_SESSION_ID").or(session);
        Self {
            host,
            client,
            cmux_socket: get("CMUX_SOCKET_PATH").or_else(|| get("CMUX_SOCKET")),
            surface: get("CMUX_SURFACE_ID"),
            agent: get("SHIPYARD_AGENT_ID"),
            session,
        }
    }

    /// Whether there is anything to deliver to: a cmux surface.
    pub(crate) fn addressable(&self) -> bool {
        self.surface.is_some()
    }

    /// The record this session registers for `repo#pr`.
    pub(crate) fn record(
        &self,
        repo: &str,
        pr: u64,
        source: Source,
        now: DateTime<Utc>,
    ) -> OwnerRecord {
        OwnerRecord {
            schema: RECORD_SCHEMA,
            repo: repo.to_owned(),
            pr,
            host: self.host.clone(),
            client: self.client.unwrap_or(Client::Other),
            cmux_socket: self.cmux_socket.clone(),
            surface: self.surface.clone(),
            agent: self.agent.clone(),
            session: self.session.clone(),
            registered_at: now,
            source,
            released: false,
        }
    }
}

/// The short name this host goes by in owner records: `SHIPYARD_HOST_NAME`,
/// else `relay.host_name` from the machine config, else the system's short
/// host name.
pub(crate) fn resolve_host_name(
    configured: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> String {
    if let Some(name) = env("SHIPYARD_HOST_NAME").filter(|v| !v.trim().is_empty()) {
        return name.trim().to_owned();
    }
    if let Some(name) = configured.filter(|v| !v.trim().is_empty()) {
        return name.trim().to_owned();
    }
    system_short_host_name()
}

fn system_short_host_name() -> String {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "unknown-host".to_owned())
}

/// A marker or store value is kept only when it is made of these characters,
/// so it can never close the HTML comment, break a line, or carry shell
/// syntax into a transport.
pub(crate) fn safe_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._:/@+-".contains(&b))
}

/// The one-line body marker for `record`. Unsafe values are left out.
pub(crate) fn render_marker(record: &OwnerRecord) -> String {
    let mut fields: Vec<(&str, String)> = vec![
        ("host", record.host.clone()),
        ("client", record.client.as_str().to_owned()),
    ];
    if let Some(surface) = &record.surface {
        fields.push(("surface", surface.clone()));
    }
    if let Some(socket) = &record.cmux_socket {
        fields.push(("socket", socket.clone()));
    }
    if let Some(session) = &record.session {
        fields.push(("session", session.clone()));
    }
    if let Some(agent) = &record.agent {
        fields.push(("agent", agent.clone()));
    }
    fields.push((
        "at",
        record
            .registered_at
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ));
    if record.released {
        fields.push(("released", "true".to_owned()));
    }
    let body = fields
        .into_iter()
        .filter(|(_, value)| safe_value(value))
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{MARKER_PREFIX} {body} {MARKER_SUFFIX}")
}

/// The owner named by the last marker in `body`, if any parses.
pub(crate) fn parse_marker(repo: &str, pr: u64, body: &str) -> Option<OwnerRecord> {
    let line = body
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.starts_with(MARKER_PREFIX) && line.ends_with(MARKER_SUFFIX))?;
    let inner = line
        .strip_prefix(MARKER_PREFIX)?
        .strip_suffix(MARKER_SUFFIX)?
        .trim();
    let mut fields = BTreeMap::new();
    for pair in inner.split_whitespace() {
        let (key, value) = pair.split_once('=')?;
        if safe_value(value) {
            fields.insert(key.to_owned(), value.to_owned());
        }
    }
    let host = fields.get("host")?.clone();
    let registered_at = fields
        .get("at")
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map_or(DateTime::<Utc>::MIN_UTC, |at| at.with_timezone(&Utc));
    Some(OwnerRecord {
        schema: RECORD_SCHEMA,
        repo: repo.to_owned(),
        pr,
        host,
        client: Client::parse(fields.get("client").map_or("", String::as_str)),
        cmux_socket: fields.get("socket").cloned(),
        surface: fields.get("surface").cloned(),
        agent: fields.get("agent").cloned(),
        session: fields.get("session").cloned(),
        registered_at,
        source: Source::BodyMarker,
        released: fields.get("released").is_some_and(|v| v == "true"),
    })
}

/// `body` with its owner marker replaced by `marker`, or `marker` appended
/// when there is none. `None` when the body already carries exactly
/// `marker`.
pub(crate) fn upsert_marker(body: &str, marker: &str) -> Option<String> {
    let mut found = false;
    let mut lines = Vec::new();
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with(MARKER_PREFIX) && trimmed.ends_with(MARKER_SUFFIX) {
            if found {
                // Keep exactly one marker.
                continue;
            }
            found = true;
            if trimmed == marker {
                lines.push(line.to_owned());
                continue;
            }
            lines.push(marker.to_owned());
            continue;
        }
        lines.push(line.to_owned());
    }
    if found {
        let next = lines.join("\n");
        let current = body.trim_end_matches('\n');
        return (next != current).then_some(next);
    }
    let trimmed = body.trim_end();
    Some(if trimmed.is_empty() {
        marker.to_owned()
    } else {
        format!("{trimmed}\n\n{marker}")
    })
}

/// Where owner records live under `state_dir`.
pub(crate) fn store_root(state_dir: &Path) -> PathBuf {
    state_dir.join("owners")
}

/// The record file for `repo#pr`, or `None` for a slug that is not exactly
/// `OWNER/REPO` of safe characters.
pub(crate) fn record_path(state_dir: &Path, repo: &str, pr: u64) -> Option<PathBuf> {
    let (owner, name) = repo.split_once('/')?;
    let ok = |part: &str| {
        !part.is_empty()
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    (ok(owner) && ok(name)).then(|| {
        store_root(state_dir)
            .join(owner)
            .join(name)
            .join(format!("{pr}.json"))
    })
}

/// Write `record` atomically.
pub(crate) fn save(state_dir: &Path, record: &OwnerRecord) -> Result<PathBuf, String> {
    let path = record_path(state_dir, &record.repo, record.pr)
        .ok_or_else(|| format!("invalid repository {:?}", record.repo))?;
    crate::durable_file::replace_json(&path, record, true)
        .map_err(|error| format!("write {}: {error}", path.display()))?;
    Ok(path)
}

/// The stored record for `repo#pr`, if one is readable.
pub(crate) fn load(state_dir: &Path, repo: &str, pr: u64) -> Option<OwnerRecord> {
    let path = record_path(state_dir, repo, pr)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Every stored record for `repo`.
pub(crate) fn load_repo(state_dir: &Path, repo: &str) -> Vec<OwnerRecord> {
    let Some(dir) = record_path(state_dir, repo, 0).and_then(|p| p.parent().map(Path::to_path_buf))
    else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records: Vec<OwnerRecord> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| std::fs::read_to_string(entry.path()).ok())
        .filter_map(|text| serde_json::from_str::<OwnerRecord>(&text).ok())
        .filter(|record| record.repo == repo)
        .collect();
    records.sort_by_key(|record| record.pr);
    records
}

/// Every stored record on this host.
pub(crate) fn load_all(state_dir: &Path) -> Vec<OwnerRecord> {
    let root = store_root(state_dir);
    let mut out = Vec::new();
    let Ok(owners) = std::fs::read_dir(&root) else {
        return out;
    };
    for owner in owners.flatten() {
        let Ok(names) = std::fs::read_dir(owner.path()) else {
            continue;
        };
        for name in names.flatten() {
            let repo = format!(
                "{}/{}",
                owner.file_name().to_string_lossy(),
                name.file_name().to_string_lossy()
            );
            out.extend(load_repo(state_dir, &repo));
        }
    }
    out
}

/// The newer of two candidate records (by `registered_at`; a tie keeps
/// `current`).
pub(crate) fn newest(current: Option<OwnerRecord>, candidate: OwnerRecord) -> OwnerRecord {
    match current {
        Some(current) if current.registered_at >= candidate.registered_at => current,
        _ => candidate,
    }
}

/// GraphQL: one pull request's node id and body.
pub(crate) const PR_BODY_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){id number state body}}}";
/// GraphQL: the open pull request whose head is `$branch`.
pub(crate) const PR_FOR_BRANCH_QUERY: &str = "query($owner:String!,$name:String!,$branch:String!){repository(owner:$owner,name:$name){pullRequests(headRefName:$branch,states:OPEN,first:2){nodes{id number state body}}}}";
/// GraphQL: replace a pull request's body.
pub(crate) const UPDATE_BODY_MUTATION: &str = "mutation($id:ID!,$body:String!){updatePullRequest(input:{pullRequestId:$id,body:$body}){pullRequest{number}}}";

/// One pull request as the registry reads it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PrBody {
    pub(crate) id: String,
    pub(crate) number: u64,
    pub(crate) state: String,
    pub(crate) body: String,
}

fn pr_body_from(node: &Value) -> Option<PrBody> {
    Some(PrBody {
        id: node.get("id")?.as_str()?.to_owned(),
        number: node.get("number")?.as_u64()?,
        state: node
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        body: node
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
    })
}

/// A `gh` runner: argv after `gh`, answer on stdout.
pub(crate) type Gh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

fn graphql_args(query: &str, fields: &[(&str, String, bool)]) -> Vec<String> {
    let mut args = vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
    ];
    for (key, value, typed) in fields {
        args.push(if *typed { "-F" } else { "-f" }.to_owned());
        args.push(format!("{key}={value}"));
    }
    args
}

fn split_repo(repo: &str) -> Result<(&str, &str), String> {
    repo.split_once('/')
        .filter(|(owner, name)| !owner.is_empty() && !name.is_empty())
        .ok_or_else(|| format!("invalid repository {repo:?}"))
}

/// Read `repo#number`'s id and body over GraphQL.
pub(crate) fn read_pr_body(gh: Gh<'_>, repo: &str, number: u64) -> Result<PrBody, String> {
    let (owner, name) = split_repo(repo)?;
    let raw = gh(&graphql_args(
        PR_BODY_QUERY,
        &[
            ("owner", owner.to_owned(), false),
            ("name", name.to_owned(), false),
            ("number", number.to_string(), true),
        ],
    ))?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("unparseable answer: {error}"))?;
    value
        .pointer("/data/repository/pullRequest")
        .and_then(pr_body_from)
        .ok_or_else(|| format!("{repo}#{number} not found"))
}

/// The open pull request whose head branch is `branch`, over GraphQL.
pub(crate) fn find_pr_for_branch(
    gh: Gh<'_>,
    repo: &str,
    branch: &str,
) -> Result<Option<PrBody>, String> {
    let (owner, name) = split_repo(repo)?;
    let raw = gh(&graphql_args(
        PR_FOR_BRANCH_QUERY,
        &[
            ("owner", owner.to_owned(), false),
            ("name", name.to_owned(), false),
            ("branch", branch.to_owned(), false),
        ],
    ))?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("unparseable answer: {error}"))?;
    let nodes = value
        .pointer("/data/repository/pullRequests/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    match nodes.as_slice() {
        [] => Ok(None),
        [one] => Ok(pr_body_from(one)),
        _ => Err(format!(
            "more than one open pull request has head {branch}; pass --pr"
        )),
    }
}

/// What a marker write did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MarkerWrite {
    /// The body now carries the marker.
    Written,
    /// It already did.
    Unchanged,
}

/// Put `record`'s marker into `pr`'s body over GraphQL.
pub(crate) fn write_marker(
    gh: Gh<'_>,
    pr: &PrBody,
    record: &OwnerRecord,
) -> Result<MarkerWrite, String> {
    let marker = render_marker(record);
    let Some(next) = upsert_marker(&pr.body, &marker) else {
        return Ok(MarkerWrite::Unchanged);
    };
    gh(&graphql_args(
        UPDATE_BODY_MUTATION,
        &[("id", pr.id.clone(), false), ("body", next, false)],
    ))?;
    Ok(MarkerWrite::Written)
}

/// What [`register`] did, for the caller to print.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Registration {
    pub(crate) record: OwnerRecord,
    pub(crate) stored_at: Result<PathBuf, String>,
    pub(crate) marker: Result<MarkerWrite, String>,
}

impl Registration {
    /// One human-readable line.
    pub(crate) fn line(&self) -> String {
        let r = &self.record;
        let address = r.surface.as_deref().unwrap_or("no cmux surface");
        let stored = match &self.stored_at {
            Ok(_) => "stored".to_owned(),
            Err(error) => format!("NOT stored ({error})"),
        };
        let marker = match &self.marker {
            Ok(MarkerWrite::Written) => "body marker written".to_owned(),
            Ok(MarkerWrite::Unchanged) => "body marker current".to_owned(),
            Err(error) => format!("body marker NOT written ({error})"),
        };
        format!(
            "▸ Owner of {}#{}: {} {} {address}{}; {stored}, {marker}",
            r.repo,
            r.pr,
            r.host,
            r.client.as_str(),
            if r.released {
                " (released to the repair lane)"
            } else {
                ""
            },
        )
    }
}

/// Record `record` durably and in `pr`'s body. Each half is attempted even
/// when the other fails; the caller reports both.
pub(crate) fn register(
    gh: Gh<'_>,
    state_dir: &Path,
    pr: &PrBody,
    record: OwnerRecord,
) -> Registration {
    let stored_at = save(state_dir, &record);
    let marker = write_marker(gh, pr, &record);
    Registration {
        record,
        stored_at,
        marker,
    }
}
