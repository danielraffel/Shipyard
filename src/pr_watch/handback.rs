//! Hand red pull requests back to the session that owns them.
//!
//! Three tiers, all behind `[pr_watch.handback] enabled` (off by default):
//!
//! - **Tier 0**: the sticky comment (see [`super::comment`]) plus the
//!   [`NEEDS_AGENT_LABEL`] label on a pull request with an *owner-actionable*
//!   flag ([`actionable`]) and an owner that resolves, removed when every such
//!   flag is addressed or the pull request merges or closes. The
//!   label is only ever added or removed, never defined: when the repository
//!   lacks it, the pass reports that and adds nothing. A label someone else
//!   put on (or took off) is never touched.
//! - **Tier 1**: when the owner's session is live on its host, a
//!   `cmux notify` on its surface (and, opt-in, a sidebar status pill) and an
//!   inbox line in `~/.local/state/shipyard/inbox/<session>.jsonl` on that
//!   host, which the Shipyard plugin's session hook shows the agent on its
//!   next turn. Nothing is ever typed into a session.
//! - **Tier 2**: an owner that is dead, unknown, or unreachable for
//!   `unowned_after_hours` is marked `unowned` on the pull request's digest
//!   line, with the resume hint from the `whence` marker.
//!
//! One delivery per (pull request, flag episode): a delivery is recorded in
//! the ledger against the episode's start, so an unchanged episode is never
//! re-sent, and a session gets at most one delivery per
//! `session_interval_minutes` (pending episodes wait for the next window).
//! In [`HandbackMode::Plan`] (the CLI default) the pass reads, probes
//! liveness (read-only), and prints what it would do; it sends nothing.

pub mod host;
pub mod owner;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::host::{HostCommand, HostRunner, Invocation, Liveness, RunError, STATUS_KEY_PREFIX};
use self::owner::{Owner, Route};
use super::flags::{DigestRoute, FlagKind};
use super::ledger::{Ledger, LedgerEntry, LedgerEvent};
use super::{RepoHistory, open_at};
use crate::config::LoadedConfig;
use crate::gate_cost::SyncGhReader;

/// The label a pull request with an owner-actionable flag carries.
pub const NEEDS_AGENT_LABEL: &str = "shipyard:needs-agent";
/// Inbox entry schema.
pub const INBOX_SCHEMA: &str = "shipyard.pr-watch.handback/v1";

/// `[pr_watch.handback]` settings.
#[allow(clippy::struct_excessive_bools)] // One switch per channel, as in the config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandbackConfig {
    /// Master switch for the daemon; the CLI can still plan with it off.
    pub enabled: bool,
    /// Tier 0 label add/remove.
    pub label: bool,
    /// Tier 1 `cmux notify`.
    pub notify: bool,
    /// Tier 1 sidebar status pill (with `notify`).
    pub status: bool,
    /// Tier 1 inbox file.
    pub inbox: bool,
    /// Stamped host name to ssh alias (or `"local"`).
    pub hosts: BTreeMap<String, String>,
    /// cmux CLI path on every host.
    pub cmux_path: String,
    /// Minimum minutes between deliveries to one session.
    pub session_interval: Duration,
    /// How long an owner must be not-live before the digest calls it unowned.
    pub unowned_after: Duration,
    /// Per host-command deadline.
    pub timeout_seconds: u64,
}

impl Default for HandbackConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            label: true,
            notify: false,
            status: false,
            inbox: false,
            hosts: BTreeMap::new(),
            cmux_path: host::DEFAULT_CMUX_PATH.to_owned(),
            session_interval: Duration::minutes(30),
            unowned_after: Duration::hours(1),
            timeout_seconds: 20,
        }
    }
}

impl HandbackConfig {
    /// Read `[pr_watch.handback]` and `[pr_watch.handback.hosts]`.
    #[must_use]
    pub fn from_config(config: &LoadedConfig) -> Self {
        let flag = |key: &str| {
            config
                .get(&format!("pr_watch.handback.{key}"))
                .and_then(toml::Value::as_bool)
        };
        let int = |key: &str| {
            config
                .get(&format!("pr_watch.handback.{key}"))
                .and_then(toml::Value::as_integer)
        };
        let defaults = Self::default();
        let hosts = config
            .get("pr_watch.handback.hosts")
            .and_then(toml::Value::as_table)
            .map(|table| {
                table
                    .iter()
                    .filter_map(|(name, alias)| Some((name.clone(), alias.as_str()?.to_owned())))
                    .collect()
            })
            .unwrap_or_default();
        Self {
            enabled: flag("enabled").unwrap_or(defaults.enabled),
            label: flag("label").unwrap_or(defaults.label),
            notify: flag("notify").unwrap_or(defaults.notify),
            status: flag("status").unwrap_or(defaults.status),
            inbox: flag("inbox").unwrap_or(defaults.inbox),
            hosts,
            cmux_path: config
                .get_str("pr_watch.handback.cmux_path")
                .map_or(defaults.cmux_path, str::to_owned),
            session_interval: int("session_interval_minutes")
                .map_or(defaults.session_interval, |m| Duration::minutes(m.max(1))),
            unowned_after: int("unowned_after_hours")
                .map_or(defaults.unowned_after, |h| Duration::hours(h.max(0))),
            timeout_seconds: int("timeout_seconds")
                .and_then(|s| u64::try_from(s.max(1)).ok())
                .unwrap_or(defaults.timeout_seconds),
        }
    }
}

/// What a pass does about hand-back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HandbackMode {
    /// Nothing.
    #[default]
    Off,
    /// Read, probe liveness, and report what would be delivered.
    Plan,
    /// Deliver through the channels the config enables.
    Deliver,
}

/// Hand-back state persisted in the ledger.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandbackState {
    /// Deliveries by ledger entry id.
    #[serde(default)]
    pub delivered: BTreeMap<String, Delivered>,
    /// Last delivery per session.
    #[serde(default)]
    pub sessions: BTreeMap<String, DateTime<Utc>>,
    /// Pull requests this tool labelled.
    #[serde(default)]
    pub labels: BTreeMap<u64, LabelRecord>,
    /// Status pills this tool set, by pull request.
    #[serde(default)]
    pub statuses: BTreeMap<u64, StatusRecord>,
    /// Latest owner observation per pull request.
    #[serde(default)]
    pub owners: BTreeMap<u64, OwnerRecord>,
    /// Open wake episodes by ledger entry id: raised, maybe sent, not yet
    /// resolved. Each transition is also a `wake.*` ledger event.
    #[serde(default)]
    pub wakes: BTreeMap<String, WakeRecord>,
}

/// One owner-actionable flag episode the hand-back is calling its owner back
/// for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WakeRecord {
    /// Pull request.
    pub pr: u64,
    /// The episode's start ([`LedgerEntry::first_seen_at`]).
    pub episode: DateTime<Utc>,
    /// When the episode was first seen by a delivering pass.
    pub raised_at: DateTime<Utc>,
    /// First successful delivery.
    #[serde(default)]
    pub sent_at: Option<DateTime<Utc>>,
    /// Why nothing was attempted for it (`no_channel`), once logged.
    #[serde(default)]
    pub unsent: Option<String>,
    /// Where its inbox line went, so the line can be retracted when the
    /// episode resolves before the owner reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inbox: Option<InboxTarget>,
}

/// The inbox an episode's line was appended to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxTarget {
    /// Session whose inbox holds the line.
    pub session: String,
    /// Host route of that inbox.
    pub route: Route,
    /// The line's `id`.
    pub line: String,
}

/// A retraction owed to an inbox: the episode its line describes resolved.
struct Retraction {
    pr: u64,
    target: InboxTarget,
    how: String,
}

/// One delivered episode.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    /// The episode's start ([`LedgerEntry::first_seen_at`]).
    pub episode: DateTime<Utc>,
    /// When.
    pub at: DateTime<Utc>,
    /// To which session.
    pub session: String,
    /// Channels that succeeded.
    pub channels: Vec<String>,
}

/// A label this tool added.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelRecord {
    /// When.
    pub added_at: DateTime<Utc>,
    /// Someone removed it since; do not add it back this episode.
    #[serde(default)]
    pub removed_by_other: bool,
}

/// A status pill this tool set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusRecord {
    /// Host route.
    pub route: Route,
    /// Workspace UUID.
    pub workspace: String,
}

/// The latest owner observation of a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnerRecord {
    /// Owner, when one was found.
    pub owner: Option<Owner>,
    /// `live`, `dead`, `unknown`, `unreachable`, or `none`.
    pub state: String,
    /// Detail for the state.
    pub detail: String,
    /// Since when the state has held.
    pub since: DateTime<Utc>,
    /// Last check.
    pub checked_at: DateTime<Utc>,
    /// Tier 2: not live for at least `unowned_after_hours`.
    pub unowned: bool,
}

/// What the hand-back needs besides GitHub.
pub struct Deps<'a> {
    /// Runs host commands.
    pub runner: &'a mut dyn HostRunner,
    /// Shipyard state directory (steward handoff records).
    pub state_dir: PathBuf,
    /// This machine's short host names.
    pub local_names: Vec<String>,
    /// This machine's steward identity.
    pub local_machine: Option<String>,
}

/// One planned or performed action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HandbackAction {
    /// Tier.
    pub tier: u8,
    /// `add_label`, `remove_label`, `notify`, `set_status`, `clear_status`,
    /// `inbox`.
    pub action: String,
    /// Pull requests it concerns.
    pub prs: Vec<u64>,
    /// Target session, for tier 1.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Exact argv (`gh` for labels, host command otherwise); `None` for a
    /// local inbox file write.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub argv: Option<Vec<String>>,
    /// Human summary (notification text, inbox file).
    pub summary: String,
    /// Sent.
    pub sent: bool,
    /// Failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One pull request's owner and tier this pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OwnerView {
    /// Pull request.
    pub pr: u64,
    /// Owner record.
    pub record: OwnerRecord,
    /// Owner-actionable episodes not yet delivered.
    pub pending: Vec<String>,
    /// 1 when a live session gets (or would get) it, 2 otherwise.
    pub tier: u8,
    /// Why nothing was delivered, if so.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
}

/// What the pass found and did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct HandbackReport {
    /// Mode.
    pub mode: HandbackMode,
    /// Owners of pull requests with owner-actionable flags.
    pub owners: Vec<OwnerView>,
    /// Actions.
    pub actions: Vec<HandbackAction>,
    /// Problems that stopped something (label missing, marker malformed).
    pub gaps: Vec<String>,
    /// `wake.*` transitions of this pass, for the ledger's event log. Empty
    /// in plan mode.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<LedgerEvent>,
}

/// Log `wake.unsent` once per episode and reason.
fn log_unsent(
    state: &mut HandbackState,
    report: &mut HandbackReport,
    now: DateTime<Utc>,
    id: &str,
    reason: &str,
    session: &str,
) {
    let Some(wake) = state.wakes.get_mut(id) else {
        return;
    };
    if wake.unsent.as_deref() == Some(reason) {
        return;
    }
    wake.unsent = Some(reason.to_owned());
    report.events.push(wake_event(
        now,
        id,
        "wake.unsent",
        json!({"pr": wake.pr, "episode": wake.episode, "reason": reason, "session": session}),
    ));
}

fn wake_event(at: DateTime<Utc>, id: &str, change: &str, detail: Value) -> LedgerEvent {
    LedgerEvent {
        at,
        id: id.to_owned(),
        change: change.to_owned(),
        evidence: String::new(),
        detail: Some(detail),
    }
}

/// This machine's short host name (`hostname -s`), for routing an owner
/// stamped with it to the local host without ssh.
#[must_use]
pub fn local_host_names() -> Vec<String> {
    std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .into_iter()
        .collect()
}

/// Whether a flag episode is something the pull request's owner can act on:
/// routed to the owner (not a shared/pre-existing failure, not a neighbour's
/// ejection) and one of the failure kinds. A rebase treadmill (main moving)
/// and the split advisory are not.
#[must_use]
pub fn actionable(entry: &LedgerEntry) -> bool {
    entry.addressed_at.is_none()
        && entry.route == DigestRoute::PerPr
        && matches!(
            entry.kind,
            FlagKind::RepeatTestFailure
                | FlagKind::RedWhileArmed
                | FlagKind::RepeatedEjection
                | FlagKind::GreenUnarmed
        )
}

fn label_add_argv(repo: &str, pr: u64) -> Vec<String> {
    vec![
        "api".to_owned(),
        "--method".to_owned(),
        "POST".to_owned(),
        format!("repos/{repo}/issues/{pr}/labels"),
        "-f".to_owned(),
        format!("labels[]={NEEDS_AGENT_LABEL}"),
    ]
}

fn label_remove_argv(repo: &str, pr: u64) -> Vec<String> {
    vec![
        "api".to_owned(),
        "--method".to_owned(),
        "DELETE".to_owned(),
        format!("repos/{repo}/issues/{pr}/labels/shipyard%3Aneeds-agent"),
    ]
}

fn label_definition_argv(repo: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        format!("repos/{repo}/labels/shipyard%3Aneeds-agent"),
    ]
}

fn not_found(error: &str) -> bool {
    error.contains("404") || error.contains("Not Found")
}

fn action(tier: u8, name: &str, prs: Vec<u64>, summary: String) -> HandbackAction {
    HandbackAction {
        tier,
        action: name.to_owned(),
        prs,
        session: None,
        argv: None,
        summary,
        sent: false,
        error: None,
    }
}

/// Tier 0: add the label where an owner-actionable flag holds, remove it
/// where none does.
fn plan_labels(
    state: &mut HandbackState,
    history: &RepoHistory,
    actionable_prs: &BTreeSet<u64>,
    gh: &SyncGhReader<'_>,
    report: &mut HandbackReport,
) -> Vec<(HandbackAction, bool)> {
    let has_label = |pr: u64| {
        history
            .prs
            .get(&pr)
            .is_some_and(|entry| entry.labels.iter().any(|l| l == NEEDS_AGENT_LABEL))
    };
    let mut out = Vec::new();
    let mut adds = Vec::new();
    for &pr in actionable_prs {
        match state.labels.get_mut(&pr) {
            Some(record) if !has_label(pr) => record.removed_by_other = true,
            None if !has_label(pr) => adds.push(pr),
            // Ours and present, or someone else's label (never ours to remove).
            Some(_) | None => {}
        }
    }
    if !adds.is_empty() {
        match gh(&label_definition_argv(&history.repo)) {
            Ok(_) => {
                for pr in adds {
                    let mut add =
                        action(0, "add_label", vec![pr], format!("add {NEEDS_AGENT_LABEL}"));
                    add.argv = Some(label_add_argv(&history.repo, pr));
                    out.push((add, true));
                }
            }
            Err(error) if not_found(&error) => report.gaps.push(format!(
                "label {NEEDS_AGENT_LABEL} does not exist in {}; not created, not added to {}",
                history.repo,
                adds.iter()
                    .map(|pr| format!("#{pr}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Err(error) => report.gaps.push(format!(
                "label {NEEDS_AGENT_LABEL} unreadable, not adding: {error}"
            )),
        }
    }
    for (&pr, record) in state
        .labels
        .iter()
        .filter(|(pr, _)| !actionable_prs.contains(pr))
    {
        let mut remove = action(
            0,
            "remove_label",
            vec![pr],
            format!("remove {NEEDS_AGENT_LABEL}"),
        );
        // A merged, closed, or unobserved pull request keeps a label nobody
        // will act on, and its snapshot can predate our own add, so ours is
        // deleted there whatever the snapshot shows (a 404 means it is gone).
        // A label a person took off stays off.
        let terminal = history
            .prs
            .get(&pr)
            .is_none_or(|entry| entry.closed_at.is_some() || entry.merged_at.is_some());
        if has_label(pr) || (terminal && !record.removed_by_other) {
            remove.argv = Some(label_remove_argv(&history.repo, pr));
            out.push((remove, false));
        } else {
            "forget (label already absent)".clone_into(&mut remove.summary);
            out.push((remove, false));
        }
    }
    out
}

/// A request that changes GitHub state (`gh` argv in, stdout out).
pub type GhWriter<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// Pages the label sweep reads at most (100 items each).
const SWEEP_MAX_PAGES: u32 = 10;

/// A `shipyard:needs-agent` label on a pull request that is no longer open.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StaleLabel {
    /// Pull request number.
    pub pr: u64,
    /// `merged` or `closed`.
    pub state: String,
    /// When it closed, as GitHub reports it.
    pub closed_at: Option<String>,
    /// Whether the DELETE succeeded (or the label was already gone).
    pub removed: bool,
    /// The DELETE's error, when it failed.
    pub error: Option<String>,
}

/// Find every closed or merged pull request still carrying the
/// [`NEEDS_AGENT_LABEL`] label, and with `write` remove it. Issues and open
/// pull requests are never touched; an open one is the regular pass's to
/// judge. Without `write` it only reads.
///
/// # Errors
/// When the listing cannot be read or parsed.
pub fn sweep_labels(
    repo: &str,
    gh: &SyncGhReader<'_>,
    write: Option<GhWriter<'_>>,
) -> Result<Vec<StaleLabel>, String> {
    let mut found = Vec::new();
    for page in 1..=SWEEP_MAX_PAGES {
        let raw = gh(&[
            "api".to_owned(),
            format!(
                "repos/{repo}/issues?labels=shipyard%3Aneeds-agent&state=closed&per_page=100&page={page}"
            ),
        ])?;
        let items: Vec<Value> =
            serde_json::from_str(&raw).map_err(|e| format!("issue listing JSON: {e}"))?;
        let count = items.len();
        for item in items {
            let Some(pull) = item.get("pull_request") else {
                continue;
            };
            let labelled = item
                .get("labels")
                .and_then(Value::as_array)
                .is_some_and(|labels| {
                    labels.iter().any(|label| {
                        label.get("name").and_then(Value::as_str) == Some(NEEDS_AGENT_LABEL)
                    })
                });
            let closed = item.get("state").and_then(Value::as_str) == Some("closed");
            let Some(pr) = item.get("number").and_then(Value::as_u64) else {
                continue;
            };
            if !labelled || !closed {
                continue;
            }
            let merged = pull.get("merged_at").is_some_and(|at| !at.is_null());
            found.push(StaleLabel {
                pr,
                state: if merged { "merged" } else { "closed" }.to_owned(),
                closed_at: item
                    .get("closed_at")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                removed: false,
                error: None,
            });
        }
        if count < 100 {
            break;
        }
    }
    found.sort_by_key(|stale| stale.pr);
    if let Some(write) = write {
        for stale in &mut found {
            match write(&label_remove_argv(repo, stale.pr)) {
                Ok(_) => stale.removed = true,
                Err(error) if not_found(&error) => stale.removed = true,
                Err(error) => stale.error = Some(error),
            }
        }
    }
    Ok(found)
}

/// Whether the pull request is open right now, read just before a label add.
/// The scan's snapshot can be minutes old, and a label posted on a pull
/// request that merged since stays there forever.
fn still_open(gh: &SyncGhReader<'_>, repo: &str, pr: u64) -> Result<bool, String> {
    let raw = gh(&["api".to_owned(), format!("repos/{repo}/pulls/{pr}")])?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| format!("pull request JSON: {e}"))?;
    let state = value.get("state").and_then(Value::as_str);
    let merged = value
        .get("merged_at")
        .is_some_and(|merged| !merged.is_null());
    match state {
        Some(state) => Ok(state == "open" && !merged),
        None => Err("pull request JSON has no state".to_owned()),
    }
}

fn fetch_whence(gh: &SyncGhReader<'_>, repo: &str, pr: u64) -> Result<Option<Owner>, String> {
    let raw = gh(&["api".to_owned(), format!("repos/{repo}/pulls/{pr}")])?;
    let value: Value = serde_json::from_str(&raw).map_err(|e| format!("pull request JSON: {e}"))?;
    let body = value
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    owner::parse_whence(body)
}

fn probe(owner: &Owner, config: &HandbackConfig, deps: &mut Deps<'_>) -> (Option<Route>, Liveness) {
    let route = match owner::route_for(owner, &config.hosts, &deps.local_names) {
        Ok(route) => route,
        Err(error) => return (None, Liveness::Unreachable(error)),
    };
    let command = HostCommand::SessionsList {
        session: owner.session.clone(),
    };
    let Some(invocation) = host::invocation(&route, &command, &config.cmux_path, None) else {
        return (Some(route), Liveness::Unknown("no invocation".to_owned()));
    };
    let liveness = match deps.runner.run(&invocation) {
        Ok(stdout) => host::parse_sessions(&stdout, &owner.session, owner.surface.as_deref()),
        Err(RunError::Unreachable(e)) => Liveness::Unreachable(e),
        Err(RunError::Failed(e)) => Liveness::Unknown(e),
    };
    (Some(route), liveness)
}

fn detail(liveness: &Liveness) -> String {
    match liveness {
        Liveness::Live { surface, .. } => format!(
            "running on surface {}",
            surface.as_deref().unwrap_or("unknown")
        ),
        Liveness::Dead(d) | Liveness::Unknown(d) | Liveness::Unreachable(d) => d.clone(),
    }
}

/// The `id` of an episode's inbox line.
fn inbox_line_id(id: &str, episode: DateTime<Utc>) -> String {
    format!("{id}@{}", episode.format("%Y-%m-%dT%H:%M:%SZ"))
}

fn inbox_line(repo: &str, id: &str, entry: &LedgerEntry, now: DateTime<Utc>) -> String {
    json!({
        "schema": INBOX_SCHEMA,
        "id": inbox_line_id(id, entry.first_seen_at),
        "repo": repo,
        "pr": entry.pr,
        "url": entry.url,
        "title": entry.title,
        "kind": entry.kind.as_str(),
        "key": entry.key,
        "verdict": entry.verdict,
        "evidence": entry.evidence,
        "head_sha": entry.head_sha,
        "first_seen_at": entry.first_seen_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        "delivered_at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    })
    .to_string()
}

fn clip(text: &str, max: usize) -> String {
    let mut out: String = text.chars().take(max).collect();
    if text.chars().count() > max {
        out.push('…');
    }
    out
}

fn notification(entries: &[(&String, &LedgerEntry)]) -> (String, String) {
    let prs: BTreeSet<u64> = entries.iter().map(|(_, e)| e.pr).collect();
    let title = if prs.len() == 1 {
        format!("Shipyard: PR #{} needs you", entries[0].1.pr)
    } else {
        format!("Shipyard: {} PRs need you", prs.len())
    };
    let body = entries
        .iter()
        .take(4)
        .map(|(_, e)| format!("#{} {}: {}", e.pr, e.verdict, clip(&e.evidence, 160)))
        .collect::<Vec<_>>()
        .join("\n");
    (title, body)
}

/// A tier-1 batch for one live session.
struct SessionBatch {
    route: Route,
    surface: Option<String>,
    workspace: Option<String>,
    ids: Vec<String>,
}

/// A line that tells the inbox hook to drop an unread line: the episode it
/// describes resolved (`how`) before the owner's next turn.
fn retraction_line(line: &str, pr: u64, how: &str, now: DateTime<Utc>) -> String {
    json!({
        "schema": INBOX_SCHEMA,
        "retract": line,
        "pr": pr,
        "reason": how,
        "at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    })
    .to_string()
}

/// Run the hand-back over a reconciled ledger. `gh` reads pull-request
/// bodies and the label definition; `write` sends label requests (only in
/// [`HandbackMode::Deliver`]).
#[allow(clippy::too_many_lines, clippy::too_many_arguments)]
pub fn run(
    ledger: &mut Ledger,
    history: &RepoHistory,
    now: DateTime<Utc>,
    config: &HandbackConfig,
    mode: HandbackMode,
    gh: &SyncGhReader<'_>,
    write: &dyn Fn(&[String]) -> Result<String, String>,
    deps: &mut Deps<'_>,
) -> HandbackReport {
    let mut report = HandbackReport {
        mode,
        ..HandbackReport::default()
    };
    if mode == HandbackMode::Off {
        return report;
    }
    let deliver = mode == HandbackMode::Deliver;
    // A plan shows every tier-1 channel, marking the ones the config leaves
    // off; a delivery uses only the enabled ones.
    let notify_on = config.notify || !deliver;
    let inbox_on = config.inbox || !deliver;
    let off_note = |enabled: bool| if enabled { "" } else { " [off in config]" };
    let mut state = ledger.handback.clone();
    let open = |pr: u64| {
        history
            .prs
            .get(&pr)
            .is_some_and(|entry| open_at(entry, now))
    };
    let mut retractions: Vec<Retraction> = Vec::new();
    let mut by_pr: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for (id, entry) in &ledger.entries {
        if actionable(entry) && open(entry.pr) {
            by_pr.entry(entry.pr).or_default().push(id.clone());
        }
    }
    // Owners first: the label calls an owner back, so a pull request whose
    // owner does not resolve gets none (it stays on the digest). One whose
    // marker could not be read this pass keeps whatever label it has.
    let mut owners: BTreeMap<u64, Option<Owner>> = BTreeMap::new();
    let mut unreadable: BTreeSet<u64> = BTreeSet::new();
    for &pr in by_pr.keys() {
        let head = history
            .prs
            .get(&pr)
            .map(|entry| entry.head_sha.clone())
            .unwrap_or_default();
        let steward = owner::steward_owner(
            &deps.state_dir,
            &history.repo,
            pr,
            &head,
            deps.local_machine.as_deref(),
        );
        let whence = match fetch_whence(gh, &history.repo, pr) {
            Ok(found) => found,
            Err(error) => {
                report.gaps.push(format!("#{pr}: whence marker: {error}"));
                unreadable.insert(pr);
                None
            }
        };
        owners.insert(pr, owner::resolve(steward, whence));
    }
    let actionable_prs: BTreeSet<u64> = by_pr.keys().copied().collect();
    let label_prs: BTreeSet<u64> = by_pr
        .keys()
        .copied()
        .filter(|pr| {
            owners.get(pr).is_some_and(Option::is_some)
                || (unreadable.contains(pr) && state.labels.contains_key(pr))
        })
        .collect();

    // Tier 0.
    if config.label {
        for (mut planned, is_add) in plan_labels(&mut state, history, &label_prs, gh, &mut report) {
            let pr = planned.prs[0];
            if deliver && is_add && planned.argv.is_some() {
                match still_open(gh, &history.repo, pr) {
                    Ok(true) => {}
                    Ok(false) => {
                        planned.argv = None;
                        planned.error =
                            Some("not added: the pull request is no longer open".to_owned());
                        report.actions.push(planned);
                        continue;
                    }
                    Err(error) => {
                        planned.argv = None;
                        planned.error = Some(format!("not added: open state unreadable: {error}"));
                        report.actions.push(planned);
                        continue;
                    }
                }
            }
            if deliver {
                let result = planned.argv.as_ref().map(|argv| write(argv));
                match result {
                    Some(Ok(_)) | None => {
                        planned.sent = planned.argv.is_some();
                        if is_add {
                            state.labels.insert(
                                pr,
                                LabelRecord {
                                    added_at: now,
                                    removed_by_other: false,
                                },
                            );
                        } else {
                            state.labels.remove(&pr);
                        }
                    }
                    Some(Err(error)) if !is_add && not_found(&error) => {
                        state.labels.remove(&pr);
                        planned.error = Some(format!("already gone: {error}"));
                    }
                    Some(Err(error)) => planned.error = Some(error),
                }
            }
            report.actions.push(planned);
        }
    }

    // Owners and liveness (read-only; recorded in both modes).
    let mut probes: BTreeMap<String, (Option<Route>, Liveness)> = BTreeMap::new();
    let mut batches: BTreeMap<String, SessionBatch> = BTreeMap::new();
    for (&pr, ids) in &by_pr {
        let found = owners.remove(&pr).flatten();
        let (route, liveness) = match &found {
            Some(o) => probes
                .entry(o.session.clone())
                .or_insert_with(|| probe(o, config, deps))
                .clone(),
            None => (
                None,
                Liveness::Unknown("no steward record or whence marker".to_owned()),
            ),
        };
        let state_name = if found.is_none() {
            "none"
        } else {
            liveness.name()
        };
        let previous = state.owners.get(&pr);
        let since = previous
            .filter(|record| {
                record.state == state_name
                    && record.owner.as_ref().map(|o| &o.session)
                        == found.as_ref().map(|o| &o.session)
            })
            .map_or(now, |record| record.since);
        let record = OwnerRecord {
            owner: found.clone(),
            state: state_name.to_owned(),
            detail: detail(&liveness),
            since,
            checked_at: now,
            unowned: state_name != "live" && now - since >= config.unowned_after,
        };
        state.owners.insert(pr, record.clone());
        if deliver {
            for id in ids {
                let Some(entry) = ledger.entries.get(id) else {
                    continue;
                };
                if let Some(previous) = state.wakes.get(id) {
                    if previous.episode == entry.first_seen_at {
                        continue;
                    }
                    // The flag cleared and came back: close the old episode.
                    if let Some(target) = previous.inbox.clone() {
                        retractions.push(Retraction {
                            pr: previous.pr,
                            target,
                            how: "new_episode".to_owned(),
                        });
                    }
                    report.events.push(wake_event(
                        now,
                        id,
                        "wake.resolved",
                        json!({
                            "pr": previous.pr,
                            "episode": previous.episode,
                            "how": "new_episode",
                            "raised_at": previous.raised_at,
                            "sent_at": previous.sent_at,
                        }),
                    ));
                }
                state.wakes.insert(
                    id.clone(),
                    WakeRecord {
                        pr: entry.pr,
                        episode: entry.first_seen_at,
                        raised_at: now,
                        sent_at: None,
                        unsent: None,
                        inbox: None,
                    },
                );
                report.events.push(wake_event(
                    now,
                    id,
                    "wake.raised",
                    json!({
                        "pr": entry.pr,
                        "kind": entry.kind.as_str(),
                        "episode": entry.first_seen_at,
                        "owner": record.state,
                        "session": record.owner.as_ref().map(|o| o.session.clone()),
                    }),
                ));
            }
        }
        let pending: Vec<String> = ids
            .iter()
            .filter(|id| {
                let episode = ledger.entries.get(*id).map(|e| e.first_seen_at);
                state
                    .delivered
                    .get(*id)
                    .is_none_or(|d| Some(d.episode) != episode)
            })
            .cloned()
            .collect();
        let mut view = OwnerView {
            pr,
            record,
            pending: pending.clone(),
            tier: 2,
            held: None,
        };
        if pending.is_empty() {
            view.held = Some("already delivered for this episode".to_owned());
            view.tier = 1;
        } else if let (Liveness::Live { surface, workspace }, Some(route), Some(o)) =
            (&liveness, route, found.as_ref())
        {
            view.tier = 1;
            let last = state.sessions.get(&o.session);
            if last.is_some_and(|at| now - *at < config.session_interval) {
                view.held = Some(format!(
                    "session rate limit: last delivery {}",
                    last.map(|at| at.format("%H:%MZ").to_string())
                        .unwrap_or_default()
                ));
            } else if !(notify_on || inbox_on) {
                view.held = Some("notify and inbox are off".to_owned());
                if deliver {
                    for id in &pending {
                        log_unsent(&mut state, &mut report, now, id, "no_channel", &o.session);
                    }
                }
            } else {
                let batch = batches.entry(o.session.clone()).or_insert(SessionBatch {
                    route,
                    surface: surface.clone().or_else(|| o.surface.clone()),
                    workspace: workspace.clone(),
                    ids: Vec::new(),
                });
                batch.ids.extend(pending);
            }
        } else {
            view.held = Some(format!("owner {}: tier 2 (digest)", view.record.state));
        }
        report.owners.push(view);
    }

    // Tier 1.
    for (session, batch) in batches {
        let entries: Vec<(&String, &LedgerEntry)> = batch
            .ids
            .iter()
            .filter_map(|id| ledger.entries.get_key_value(id))
            .collect();
        if entries.is_empty() {
            continue;
        }
        let prs: Vec<u64> = entries
            .iter()
            .map(|(_, e)| e.pr)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let mut channels = Vec::new();
        let actions_before = report.actions.len();
        let mut run_host = |name: &str,
                            command: HostCommand,
                            stdin: Option<String>,
                            summary: String,
                            report: &mut HandbackReport|
         -> bool {
            let invocation: Option<Invocation> =
                host::invocation(&batch.route, &command, &config.cmux_path, stdin.clone());
            let mut planned = action(1, name, prs.clone(), summary);
            planned.session = Some(session.clone());
            planned.argv = invocation.as_ref().map(|i| i.argv.clone());
            let mut ok = false;
            if deliver {
                let result = match (&invocation, &command) {
                    (Some(invocation), _) => deps
                        .runner
                        .run(invocation)
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                    (None, HostCommand::InboxAppend { session }) => deps
                        .runner
                        .append_local_inbox(session, stdin.as_deref().unwrap_or_default()),
                    (None, _) => Err("no invocation".to_owned()),
                };
                match result {
                    Ok(()) => {
                        planned.sent = true;
                        ok = true;
                    }
                    Err(error) => planned.error = Some(error),
                }
            }
            report.actions.push(planned);
            ok
        };
        if notify_on {
            if let Some(surface) = &batch.surface {
                let (title, body) = notification(&entries);
                if run_host(
                    "notify",
                    HostCommand::Notify {
                        surface: surface.clone(),
                        title: title.clone(),
                        body: body.clone(),
                    },
                    None,
                    format!(
                        "{title} | {}{}",
                        body.replace('\n', " | "),
                        off_note(config.notify)
                    ),
                    &mut report,
                ) {
                    channels.push("notify".to_owned());
                }
            } else {
                report
                    .gaps
                    .push(format!("session {session}: no cmux surface, not notifying"));
            }
            if config.status
                && let Some(workspace) = &batch.workspace
            {
                for pr in &prs {
                    let key = format!("{STATUS_KEY_PREFIX}{pr}");
                    if run_host(
                        "set_status",
                        HostCommand::SetStatus {
                            workspace: workspace.clone(),
                            key,
                            value: format!("PR #{pr} red"),
                        },
                        None,
                        format!("sidebar pill PR #{pr} red"),
                        &mut report,
                    ) && deliver
                    {
                        state.statuses.insert(
                            *pr,
                            StatusRecord {
                                route: batch.route.clone(),
                                workspace: workspace.clone(),
                            },
                        );
                    }
                }
            }
        }
        let place = match &batch.route {
            Route::Local => "local".to_owned(),
            Route::Ssh(alias) => format!("ssh {alias}"),
        };
        if inbox_on {
            let lines: String = entries
                .iter()
                .map(|(id, e)| inbox_line(&ledger.repo, id, e, now) + "\n")
                .collect();
            if run_host(
                "inbox",
                HostCommand::InboxAppend {
                    session: session.clone(),
                },
                Some(lines),
                format!(
                    "{} line(s) -> {place}:~/.local/state/shipyard/inbox/{session}.jsonl{}",
                    entries.len(),
                    off_note(config.inbox)
                ),
                &mut report,
            ) {
                channels.push("inbox".to_owned());
            }
        }
        // A wake with no channel attempted (notify on but no surface, inbox
        // off) was never tried: it is unsent, not failed.
        let attempted = report.actions[actions_before..]
            .iter()
            .any(|a| a.action == "notify" || a.action == "inbox");
        if deliver && channels.is_empty() && !attempted {
            for (id, _) in &entries {
                log_unsent(&mut state, &mut report, now, id, "no_channel", &session);
            }
        } else if deliver {
            let change = if channels.is_empty() {
                "wake.failed"
            } else {
                "wake.sent"
            };
            for (id, entry) in &entries {
                if !channels.is_empty()
                    && let Some(wake) = state.wakes.get_mut(*id)
                {
                    wake.sent_at.get_or_insert(now);
                    if channels.iter().any(|c| c == "inbox") {
                        wake.inbox = Some(InboxTarget {
                            session: session.clone(),
                            route: batch.route.clone(),
                            line: inbox_line_id(id, entry.first_seen_at),
                        });
                    }
                }
                report.events.push(wake_event(
                    now,
                    id,
                    change,
                    json!({
                        "pr": entry.pr,
                        "episode": entry.first_seen_at,
                        "rung": "1",
                        "channels": channels,
                        "session": session,
                        "host": place,
                    }),
                ));
            }
        }
        if deliver && !channels.is_empty() {
            for (id, entry) in &entries {
                state.delivered.insert(
                    (*id).clone(),
                    Delivered {
                        episode: entry.first_seen_at,
                        at: now,
                        session: session.clone(),
                        channels: channels.clone(),
                    },
                );
            }
            state.sessions.insert(session.clone(), now);
        }
    }

    // Clear status pills of pull requests with nothing left to act on.
    let cleared_prs: Vec<u64> = state
        .statuses
        .keys()
        .filter(|pr| !actionable_prs.contains(pr))
        .copied()
        .collect();
    for pr in cleared_prs {
        let Some(record) = state.statuses.get(&pr).cloned() else {
            continue;
        };
        let command = HostCommand::ClearStatus {
            workspace: record.workspace.clone(),
            key: format!("{STATUS_KEY_PREFIX}{pr}"),
        };
        let invocation = host::invocation(&record.route, &command, &config.cmux_path, None);
        let mut planned = action(
            1,
            "clear_status",
            vec![pr],
            format!("clear sidebar pill for #{pr}"),
        );
        planned.argv = invocation.as_ref().map(|i| i.argv.clone());
        if deliver {
            match invocation.map(|i| deps.runner.run(&i)) {
                Some(Ok(_)) => {
                    planned.sent = true;
                    state.statuses.remove(&pr);
                }
                Some(Err(RunError::Failed(e))) => {
                    // The workspace is gone; nothing left to clear.
                    planned.error = Some(e);
                    state.statuses.remove(&pr);
                }
                Some(Err(e)) => planned.error = Some(e.to_string()),
                None => {
                    state.statuses.remove(&pr);
                }
            }
        }
        report.actions.push(planned);
    }

    // An episode that is no longer owner-actionable on an open pull request
    // is resolved: addressed, closed, or pruned from the ledger.
    if deliver {
        let open_ids: BTreeSet<&String> = by_pr.values().flatten().collect();
        let resolved: Vec<String> = state
            .wakes
            .keys()
            .filter(|id| !open_ids.contains(id))
            .cloned()
            .collect();
        for id in resolved {
            let Some(wake) = state.wakes.remove(&id) else {
                continue;
            };
            let entry = ledger.entries.get(&id);
            let how = match entry {
                None => "gone",
                Some(entry) if entry.addressed_at.is_some() => "addressed",
                Some(_) if !open(wake.pr) => "closed",
                Some(_) => "not_actionable",
            };
            if let Some(target) = wake.inbox.clone() {
                retractions.push(Retraction {
                    pr: wake.pr,
                    target,
                    how: how.to_owned(),
                });
            }
            report.events.push(wake_event(
                now,
                &id,
                "wake.resolved",
                json!({
                    "pr": wake.pr,
                    "episode": wake.episode,
                    "how": how,
                    "raised_at": wake.raised_at,
                    "sent_at": wake.sent_at,
                }),
            ));
        }
    }

    // Retract unread inbox lines whose episode resolved, so the owner's next
    // turn is not told about a head it already replaced. A line the hook has
    // already shown is unaffected: the retraction only drops unread lines.
    if deliver && inbox_on {
        for retraction in retractions {
            let Retraction { pr, target, how } = retraction;
            let line = retraction_line(&target.line, pr, &how, now) + "\n";
            let command = HostCommand::InboxAppend {
                session: target.session.clone(),
            };
            let invocation = host::invocation(
                &target.route,
                &command,
                &config.cmux_path,
                Some(line.clone()),
            );
            let mut planned = action(
                1,
                "retract_inbox",
                vec![pr],
                format!("retract unread note for #{pr} ({how})"),
            );
            planned.session = Some(target.session.clone());
            planned.argv = invocation.as_ref().map(|i| i.argv.clone());
            let result = match &invocation {
                Some(invocation) => deps
                    .runner
                    .run(invocation)
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                None => deps.runner.append_local_inbox(&target.session, &line),
            };
            match result {
                Ok(()) => planned.sent = true,
                Err(error) => planned.error = Some(error),
            }
            report.actions.push(planned);
        }
    }

    // Housekeeping.
    state
        .delivered
        .retain(|id, _| ledger.entries.contains_key(id));
    state.owners.retain(|pr, _| actionable_prs.contains(pr));
    let horizon = now - Duration::days(7);
    state.sessions.retain(|_, at| *at > horizon);
    if deliver {
        ledger.handback = state;
    } else {
        // Plan mode records only observations (owners), never deliveries.
        ledger.handback.owners = state.owners;
    }
    report
}

#[cfg(test)]
mod tests;
