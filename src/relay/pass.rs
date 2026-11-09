//! One relay pass over the configured repositories, with every side effect
//! injected: GitHub through a `gh` closure, cmux through a
//! [`super::transport::CmuxRunner`], time as an argument. The caller persists
//! the returned state and appends the returned log records.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::config::{Address, Mode, RelayConfig};
use super::transport::{CmuxRunner, Routes};
use super::{ActedRecord, DeliveryRecord, PrDigest, RelayEvent, derive_events, render_message};
use crate::owner_registry::{self, Gh, OwnerRecord};

/// How long the relay waits for an owner to act on a delivery before logging
/// `none`.
const ACT_WINDOW_HOURS: i64 = 24;
/// How long a finished pull request stays in the state, so a stored owner
/// record does not bring it back as new.
const DONE_RETENTION_DAYS: i64 = 30;

/// GraphQL: post one comment on a pull request (the last fallback).
pub(crate) const ADD_COMMENT_MUTATION: &str = "mutation($id:ID!,$body:String!){addComment(input:{subjectId:$id,body:$body}){commentEdge{node{id}}}}";

/// A delivery the owner has not visibly acted on yet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Awaiting {
    pub(crate) delivered_at: DateTime<Utc>,
    pub(crate) head: String,
    pub(crate) last_comment_at: Option<String>,
    pub(crate) keys: Vec<String>,
}

/// What the relay keeps about one owned pull request between passes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Tracked {
    pub(crate) repo: String,
    pub(crate) pr: u64,
    #[serde(default)]
    pub(crate) digest: Option<PrDigest>,
    #[serde(default)]
    pub(crate) owner: Option<OwnerRecord>,
    #[serde(default)]
    pub(crate) last_sent_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(crate) pending: Vec<RelayEvent>,
    #[serde(default)]
    pub(crate) sent: BTreeSet<String>,
    #[serde(default)]
    pub(crate) awaiting: Vec<Awaiting>,
    /// Merged or closed and delivered: no longer read.
    #[serde(default)]
    pub(crate) done_at: Option<DateTime<Utc>>,
    /// Heads that already got a fallback comment: at most one per head.
    #[serde(default)]
    pub(crate) commented_heads: BTreeSet<String>,
}

/// The relay's durable state.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RelayState {
    #[serde(default)]
    pub(crate) prs: BTreeMap<String, Tracked>,
    #[serde(default)]
    pub(crate) routes: Routes,
}

/// The steward's `next_action` per pull request (head, text), for a repo.
pub(crate) type StewardActions<'a> = dyn Fn(&str) -> BTreeMap<u64, (String, String)> + 'a;

/// What a pass needs from its caller.
pub(crate) struct PassInput<'a> {
    pub(crate) config: &'a RelayConfig,
    pub(crate) repos: &'a [String],
    pub(crate) local_host: &'a str,
    pub(crate) mode: Mode,
    /// Where this host's owner records live.
    pub(crate) state_dir: &'a Path,
    pub(crate) now: DateTime<Utc>,
    pub(crate) gh: Gh<'a>,
    pub(crate) runner: &'a dyn CmuxRunner,
    /// The steward's `next_action` per pull request (head, text), by repo.
    pub(crate) steward: &'a StewardActions<'a>,
}

/// One repository's share of a pass.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct RepoPass {
    pub(crate) repo: String,
    pub(crate) tracked: usize,
    pub(crate) events: usize,
    pub(crate) deliveries: usize,
    pub(crate) undelivered: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(crate) errors: Vec<String>,
}

/// What a pass did; `log` is appended to the delivery log by the caller.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct PassReport {
    pub(crate) mode: String,
    pub(crate) repos: Vec<RepoPass>,
    #[serde(skip)]
    pub(crate) log: Vec<Value>,
}

fn key(repo: &str, pr: u64) -> String {
    format!("{repo}#{pr}")
}

fn later(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left > right,
        (Some(_), None) => true,
        _ => false,
    }
}

/// Run one pass, updating `state`.
pub(crate) fn run_pass(input: &PassInput<'_>, state: &mut RelayState) -> PassReport {
    let mut report = PassReport {
        mode: input.mode.as_str().to_owned(),
        ..PassReport::default()
    };
    for repo in input.repos {
        let mut repo_pass = RepoPass {
            repo: repo.clone(),
            ..RepoPass::default()
        };
        if let Err(error) = run_repo(input, state, repo, &mut repo_pass, &mut report.log) {
            repo_pass.errors.push(error);
        }
        report.repos.push(repo_pass);
    }
    let horizon = input.now - Duration::days(DONE_RETENTION_DAYS);
    state
        .prs
        .retain(|_, tracked| tracked.done_at.is_none_or(|at| at > horizon));
    report
}

fn adopt_owner(tracked: &mut Tracked, candidate: OwnerRecord) {
    tracked.owner = Some(owner_registry::newest(tracked.owner.take(), candidate));
}

#[allow(clippy::too_many_lines)]
fn run_repo(
    input: &PassInput<'_>,
    state: &mut RelayState,
    repo: &str,
    repo_pass: &mut RepoPass,
    log: &mut Vec<Value>,
) -> Result<(), String> {
    // Owners registered on this host.
    for record in owner_registry::load_repo(input.state_dir, repo) {
        let entry = state
            .prs
            .entry(key(repo, record.pr))
            .or_insert_with(|| Tracked {
                repo: repo.to_owned(),
                pr: record.pr,
                ..Tracked::default()
            });
        adopt_owner(entry, record);
    }
    let live: Vec<u64> = state
        .prs
        .values()
        .filter(|t| t.repo == repo && t.done_at.is_none())
        .map(|t| t.pr)
        .collect();
    let mut read = super::fetch(input.gh, repo, &live, true)?;
    // Owners registered anywhere, from the body markers.
    let mut discovered = Vec::new();
    for (number, body) in &read.markers {
        let Some(record) = owner_registry::parse_marker(repo, *number, body) else {
            continue;
        };
        let entry = state
            .prs
            .entry(key(repo, *number))
            .or_insert_with(|| Tracked {
                repo: repo.to_owned(),
                pr: *number,
                ..Tracked::default()
            });
        if entry.done_at.is_some() {
            continue;
        }
        adopt_owner(entry, record);
        if !read.prs.contains_key(number) {
            discovered.push(*number);
        }
    }
    if !discovered.is_empty() {
        let extra = super::fetch(input.gh, repo, &discovered, false)?;
        read.prs.extend(extra.prs);
    }
    let steward = (input.steward)(repo);
    let window = Duration::minutes(input.config.rate_limit_minutes);
    for (number, node) in &read.prs {
        let Some(tracked) = state.prs.get_mut(&key(repo, *number)) else {
            continue;
        };
        if tracked.done_at.is_some() {
            continue;
        }
        repo_pass.tracked += 1;
        let digest = super::summarize(
            node.as_ref(),
            &read.default_branch,
            &input.config.review_gates,
        );
        infer_actions(tracked, &digest, input.now, log);
        let events: Vec<RelayEvent> = derive_events(tracked.digest.as_ref(), &digest)
            .into_iter()
            .filter(|event| input.config.events.contains(event.name()))
            .collect();
        repo_pass.events += events.len();
        for event in events {
            let event_key = event.key(repo, *number);
            if tracked.sent.contains(&event_key)
                || tracked
                    .pending
                    .iter()
                    .any(|p| p.key(repo, *number) == event_key)
            {
                continue;
            }
            tracked.pending.push(event);
        }
        let first_sighting = tracked.digest.is_none();
        tracked.digest = Some(digest.clone());
        if tracked.pending.is_empty() {
            if digest.terminal() && (first_sighting || tracked.awaiting.is_empty()) {
                tracked.done_at = Some(input.now);
            }
            continue;
        }
        let due = tracked.pending.iter().any(RelayEvent::terminal)
            || tracked
                .last_sent_at
                .is_none_or(|at| input.now - at >= window);
        if !due {
            continue;
        }
        let steward_next = steward
            .get(number)
            .filter(|(head, _)| *head == digest.head)
            .map(|(_, text)| text.as_str());
        let record = deliver(input, &mut state.routes, tracked, &digest, steward_next);
        if record.delivered {
            repo_pass.deliveries += 1;
        } else {
            repo_pass.undelivered += 1;
        }
        log.push(serde_json::to_value(&record).unwrap_or(Value::Null));
        if record.delivered || record.refused.is_some() {
            tracked.sent.extend(record.keys.iter().cloned());
            tracked.pending.clear();
            tracked.last_sent_at = Some(input.now);
            if record.tier == "comment" && input.mode == Mode::Send {
                tracked.commented_heads.insert(digest.head.clone());
            }
            if record.delivered && input.mode == Mode::Send {
                tracked.awaiting.push(Awaiting {
                    delivered_at: input.now,
                    head: digest.head.clone(),
                    last_comment_at: digest.last_comment_at.clone(),
                    keys: record.keys.clone(),
                });
            }
            if digest.terminal() {
                tracked.done_at = Some(input.now);
            }
        }
    }
    Ok(())
}

fn infer_actions(
    tracked: &mut Tracked,
    digest: &PrDigest,
    now: DateTime<Utc>,
    log: &mut Vec<Value>,
) {
    let mut still = Vec::new();
    for awaiting in tracked.awaiting.drain(..) {
        let action = if digest.state == "MERGED" {
            Some("merged")
        } else if digest.state == "CLOSED" {
            Some("closed")
        } else if !digest.head.is_empty() && digest.head != awaiting.head {
            Some("push")
        } else if later(
            digest.last_comment_at.as_deref(),
            awaiting.last_comment_at.as_deref(),
        ) {
            Some("comment")
        } else if now - awaiting.delivered_at > Duration::hours(ACT_WINDOW_HOURS) {
            Some("none")
        } else {
            None
        };
        let Some(action) = action else {
            still.push(awaiting);
            continue;
        };
        let record = ActedRecord {
            kind: "acted".to_owned(),
            repo: tracked.repo.clone(),
            pr: tracked.pr,
            delivered_at: awaiting.delivered_at,
            acted_at: (action != "none").then_some(now),
            action: action.to_owned(),
            outcome: format!(
                "state={} merge={} checks={:?} head={}",
                digest.state,
                digest.merge,
                digest.checks,
                digest.short_head()
            ),
            keys: awaiting.keys,
        };
        log.push(serde_json::to_value(&record).unwrap_or(Value::Null));
    }
    tracked.awaiting = still;
}

/// The addresses to try, in order, with the tier each one is.
pub(crate) fn chain(
    config: &RelayConfig,
    repo: &str,
    owner: Option<&OwnerRecord>,
) -> Vec<(&'static str, Address)> {
    let mut out = Vec::new();
    if let Some(owner) = owner.filter(|o| !o.released)
        && let Some(surface) = &owner.surface
    {
        out.push((
            "owner",
            Address {
                host: owner.host.clone(),
                surface: surface.clone(),
                socket: owner.cmux_socket.clone(),
            },
        ));
    }
    let fallbacks = config.fallbacks(repo);
    if let Some(lead) = fallbacks.lead {
        out.push(("lead", lead));
    }
    if let Some(lane) = fallbacks.repair_lane {
        out.push(("repair_lane", lane));
    }
    out
}

fn deliver(
    input: &PassInput<'_>,
    routes: &mut Routes,
    tracked: &Tracked,
    digest: &PrDigest,
    steward_next: Option<&str>,
) -> DeliveryRecord {
    let repo = tracked.repo.as_str();
    let pr = tracked.pr;
    let events = tracked.pending.clone();
    let keys: Vec<String> = events.iter().map(|e| e.key(repo, pr)).collect();
    let message = render_message(repo, pr, digest, &events, steward_next);
    let mut record = DeliveryRecord {
        kind: "delivery".to_owned(),
        at: input.now,
        mode: input.mode.as_str().to_owned(),
        repo: repo.to_owned(),
        pr,
        head: digest.head.clone(),
        events,
        keys,
        tier: "none".to_owned(),
        transport: None,
        delivered: false,
        attempts: Vec::new(),
        refused: None,
        message: message.clone(),
    };
    if super::refuses_verdict_text(&message) {
        record.refused = Some("message contains verdict text; never sent".to_owned());
        return record;
    }
    let chain = chain(input.config, repo, tracked.owner.as_ref());
    if input.mode == Mode::Shadow {
        match chain.first() {
            Some((tier, address)) => {
                (*tier).clone_into(&mut record.tier);
                record.transport = Some(format!("shadow {}/{}", address.host, address.surface));
            }
            None if input.config.comment_fallback => {
                "comment".clone_into(&mut record.tier);
                record.transport = Some("shadow pr comment".to_owned());
            }
            None => {}
        }
        record.delivered = record.transport.is_some();
        return record;
    }
    for (tier, address) in &chain {
        match super::transport::send(
            input.runner,
            input.config,
            input.local_host,
            address,
            &message,
            routes,
        ) {
            Ok(transport) => {
                (*tier).clone_into(&mut record.tier);
                record.transport = Some(transport);
                record.delivered = true;
                return record;
            }
            Err(error) => record.attempts.push(format!("{tier}: {error}")),
        }
    }
    if input.config.comment_fallback
        && !digest.node_id.is_empty()
        && tracked.commented_heads.contains(&digest.head)
    {
        // One fallback comment per head; past that, drop rather than spam.
        record.refused = Some(
            "no session reachable and this head already has a relay comment; dropped".to_owned(),
        );
        return record;
    }
    if input.config.comment_fallback && !digest.node_id.is_empty() {
        let body = format!(
            "{message}\n\n(Relayed as a comment: the owning session could not be reached over cmux.)"
        );
        let args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={ADD_COMMENT_MUTATION}"),
            "-f".to_owned(),
            format!("id={}", digest.node_id),
            "-f".to_owned(),
            format!("body={body}"),
        ];
        match (input.gh)(&args) {
            Ok(_) => {
                "comment".clone_into(&mut record.tier);
                record.transport = Some("pr comment".to_owned());
                record.delivered = true;
            }
            Err(error) => record.attempts.push(format!("comment: {error}")),
        }
    }
    record
}
