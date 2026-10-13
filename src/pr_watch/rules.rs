//! Rules-first watcher: one JSONL event per new fact a close monitor would act
//! on, and nothing otherwise.
//!
//! [`evaluate`] lists the conditions that hold at one instant, each under a
//! stable key. [`RuleLedger`] remembers every key already emitted, so each
//! condition fires once; a pass that finds nothing new prints nothing. A live
//! pass is `evaluate(input, now)` folded into the persisted ledger; [`replay`]
//! primes a throwaway ledger at the window start and folds every tick, so
//! replay and live read the same rules.
//!
//! The rules, by event kind:
//!
//! | kind | holds when |
//! |---|---|
//! | `ready_unreviewed` | a comment whose first line says READY names the current head, and no verdict names it |
//! | `head_unreviewed` | opt-in (`head_unreviewed_after`): the current head has been up that long with neither a READY nor a verdict naming it |
//! | `verdict_posted` | a reviewer verdict (`verdict: APPROVE/HOLD head:<sha>`, `reviewed: <sha>`, or a review) was posted; route it to the author |
//! | `armed_unapproved` | the pull request is armed or queued and the latest verdict on its head is not an approval |
//! | `required_red` | the pull request is armed, queued or approved, and the latest attempt of a required check on its head failed or was cancelled; carries the failing test names |
//! | `group_red` | a merge group named for the pull request failed a required job; carries the failing test names |
//! | `queue_removed` | the merge queue removed the pull request for any reason but a merge |
//! | `queue_stuck` | the pull request has sat in the merge queue for `queue_stuck_after` |
//! | `approved_unarmed` | the latest verdict on the head approves it, for `approved_unarmed_after`, and nothing armed or queued it |
//! | `approved_blocked` | the head is approved but GitHub reports it `DIRTY` or `BEHIND` (live passes only: GitHub keeps no history of it) |
//! | `host_alarm` | an alarm issue (a tartci `[tartci] ...` issue) was opened |
//! | anything else | an input event (`--input-events`), passed through under its own key |
//!
//! Nothing here writes to GitHub.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use self::fix::Fix;
use super::{CheckFact, PrHistory, QueueEventKind, RepoHistory, head_at, open_at, short};

pub mod fix;
pub mod read;

/// Schema of one emitted event line.
pub const EVENT_SCHEMA: &str = "shipyard.pr-watch.rules/v1";
/// Ledger schema.
pub const LEDGER_SCHEMA: &str = "shipyard.pr-watch.rules-ledger/v1";
/// Keys older than this are dropped from the ledger.
const LEDGER_KEEP_DAYS: i64 = 14;

/// One entry of a pull request's review trail: a comment or a review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrailEntry {
    /// Comment or review id.
    pub id: u64,
    /// Author login.
    pub login: String,
    /// The author's `author_association` on the repository, when read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub association: Option<String>,
    /// When it was posted.
    pub at: DateTime<Utc>,
    /// A pull-request review rather than a comment.
    #[serde(default)]
    pub review: bool,
    /// The verdict it carries, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The head a READY announcement names, if it is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ready_head: Option<String>,
}

/// A verdict on one head.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// `true` for APPROVE (or `reviewed:`), `false` for HOLD.
    pub approve: bool,
    /// The head it names.
    pub head: String,
}

impl TrailEntry {
    /// Read a comment body: its verdict line and any READY announcement.
    #[must_use]
    pub fn from_comment(id: u64, login: &str, at: DateTime<Utc>, body: &str) -> Self {
        Self {
            id,
            login: login.to_owned(),
            association: None,
            at,
            review: false,
            verdict: crate::head_approval::verdict_line(body)
                .map(|(approve, head)| Verdict { approve, head }),
            ready_head: ready_head(body),
        }
    }
}

/// The head a READY announcement names: the first non-blank line starts with
/// the word READY (any case, after markup) and a full SHA appears in it.
#[must_use]
pub fn ready_head(body: &str) -> Option<String> {
    let line = body.lines().find(|line| !line.trim().is_empty())?;
    let words = line.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let first = words
        .split(|c: char| !c.is_ascii_alphanumeric())
        .next()
        .unwrap_or_default();
    if !first.eq_ignore_ascii_case("ready") {
        return None;
    }
    first_sha(line)
}

fn first_sha(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        let run = bytes[start..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            .count();
        if run == 40 {
            return Some(text[start..start + 40].to_owned());
        }
        start += run.max(1);
    }
    None
}

/// An alarm issue (tartci's host alerts).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Alarm {
    /// Repository that holds the issue.
    pub repo: String,
    /// Issue number.
    pub number: u64,
    /// Title.
    pub title: String,
    /// Opened.
    pub opened_at: DateTime<Utc>,
    /// Closed, if it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
}

/// An event from another source (`--input-events`), passed through once.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputEvent {
    /// Event kind.
    pub kind: String,
    /// Stable key; the event fires once per key.
    pub key: String,
    /// When it happened.
    pub at: DateTime<Utc>,
    /// Everything else on the line.
    #[serde(flatten)]
    pub rest: BTreeMap<String, Value>,
}

/// Everything the rules read.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WatchInput {
    /// One history per watched repository.
    pub repos: Vec<RepoHistory>,
    /// Review trails, by repository and pull request.
    pub trails: BTreeMap<String, BTreeMap<u64, Vec<TrailEntry>>>,
    /// Alarm issues.
    pub alarms: Vec<Alarm>,
    /// Events from other sources.
    pub inputs: Vec<InputEvent>,
}

/// Rule timings, and who reviews each repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleConfig {
    /// A head with no READY and no verdict is reported after this long;
    /// `None` turns the rule off (it fires for every head an agent is still
    /// working on, so it is opt-in).
    pub head_unreviewed_after: Option<Duration>,
    /// A merge-queue entry is stuck after this long.
    pub queue_stuck_after: Duration,
    /// An approved head nobody armed is reported after this long.
    pub approved_unarmed_after: Duration,
    /// The reviewer to route each repository's heads to, by `OWNER/REPO`
    /// (lower-cased); a fix names it.
    pub reviewers: BTreeMap<String, String>,
}

impl RuleConfig {
    fn reviewer(&self, repo: &str) -> Option<&str> {
        self.reviewers
            .get(&repo.to_ascii_lowercase())
            .map(String::as_str)
    }
}

impl Default for RuleConfig {
    fn default() -> Self {
        Self {
            head_unreviewed_after: None,
            queue_stuck_after: Duration::minutes(90),
            approved_unarmed_after: Duration::minutes(15),
            reviewers: BTreeMap::new(),
        }
    }
}

/// One condition that holds, or one event to emit.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RuleEvent {
    /// [`EVENT_SCHEMA`].
    pub schema: String,
    /// Event kind.
    pub kind: String,
    /// Stable key; the event fires once per key.
    pub key: String,
    /// When the pass that emitted it ran (replay: the tick).
    pub at: DateTime<Utc>,
    /// When the condition began, where the data says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Pull request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
    /// Head.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// One line a person or an agent can act on.
    pub summary: String,
    /// Structured facts behind it.
    #[serde(default)]
    pub detail: Value,
    /// What to do about it.
    #[serde(default)]
    pub fix: Fix,
}

struct PrState {
    armed: bool,
    queued_since: Option<DateTime<Utc>>,
    armed_since: Option<DateTime<Utc>>,
}

fn pr_state(pr: &PrHistory, at: DateTime<Utc>) -> PrState {
    let mut state = PrState {
        armed: false,
        queued_since: None,
        armed_since: None,
    };
    for event in pr.events.iter().filter(|event| event.at <= at) {
        match &event.kind {
            QueueEventKind::Armed => {
                state.armed = true;
                state.armed_since = Some(event.at);
            }
            QueueEventKind::Disarmed { .. } => {
                state.armed = false;
                state.armed_since = None;
            }
            QueueEventKind::Enqueued => state.queued_since = Some(event.at),
            QueueEventKind::Removed { .. } => state.queued_since = None,
            QueueEventKind::Merged | QueueEventKind::Closed => {
                state.armed = false;
                state.armed_since = None;
                state.queued_since = None;
            }
            QueueEventKind::ForcePushed { .. } | QueueEventKind::Reopened => {}
        }
    }
    state
}

/// The latest verdict naming `head`, posted at or before `at`.
/// Whether `entry`'s verdict counts for a pull request by `author`: the
/// rule the arm guard applies (`head_approval::trusted_reviewer`), so a
/// drive-by comment on a public repository, or the author's own, neither
/// approves a head nor reaches the author as a reviewer's verdict.
fn trusted(entry: &TrailEntry, author: Option<&str>) -> bool {
    crate::head_approval::trusted_reviewer(&entry.login, entry.association.as_deref(), author)
}

fn latest_verdict<'a>(
    trail: &'a [TrailEntry],
    head: &str,
    at: DateTime<Utc>,
    author: Option<&str>,
) -> Option<&'a TrailEntry> {
    trail
        .iter()
        .filter(|entry| entry.at <= at && trusted(entry, author))
        .filter(|entry| {
            entry
                .verdict
                .as_ref()
                .is_some_and(|verdict| verdict.head.eq_ignore_ascii_case(head))
        })
        .max_by_key(|entry| (entry.at, entry.id))
}

fn stamp(at: DateTime<Utc>) -> String {
    at.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// The latest attempt of each required check on a head, as of `at`.
fn latest_checks(checks: &[CheckFact], at: DateTime<Utc>) -> Vec<&CheckFact> {
    let mut latest: BTreeMap<&str, &CheckFact> = BTreeMap::new();
    for check in checks {
        let began = check.started_at.or(check.completed_at);
        if began.is_none_or(|began| began > at) {
            continue;
        }
        let newer = latest.get(check.name.as_str()).is_none_or(|current| {
            (current.started_at.or(current.completed_at), current.id) < (began, check.id)
        });
        if newer {
            latest.insert(check.name.as_str(), check);
        }
    }
    latest.into_values().collect()
}

fn red(check: &CheckFact) -> bool {
    matches!(
        check.conclusion.as_deref(),
        Some("failure" | "timed_out" | "startup_failure" | "cancelled")
    )
}

fn tests_text(tests: &[String]) -> String {
    if tests.is_empty() {
        "no failing test read from the log".to_owned()
    } else {
        format!("failing: {}", tests.join(", "))
    }
}

struct Emit<'a> {
    out: &'a mut Vec<RuleEvent>,
    at: DateTime<Utc>,
    repo: &'a str,
}

impl Emit<'_> {
    #[allow(clippy::too_many_arguments)] // One event's fields, named at each call.
    fn push(
        &mut self,
        kind: &str,
        key: String,
        since: Option<DateTime<Utc>>,
        pr: &PrHistory,
        head: Option<&str>,
        summary: &str,
        detail: Value,
        fix: Fix,
    ) {
        self.out.push(RuleEvent {
            schema: EVENT_SCHEMA.to_owned(),
            kind: kind.to_owned(),
            key,
            at: self.at,
            since,
            repo: Some(self.repo.to_owned()),
            pr: Some(pr.number),
            head: head.map(str::to_owned),
            summary: format!("{}#{}: {summary}", self.repo, pr.number),
            detail,
            fix,
        });
    }
}

/// Every condition that holds at `at`, each under its key. `live` adds the
/// rules that read current-only GitHub state.
#[must_use]
pub fn evaluate(
    input: &WatchInput,
    at: DateTime<Utc>,
    config: &RuleConfig,
    live: bool,
) -> Vec<RuleEvent> {
    let mut out = Vec::new();
    for history in &input.repos {
        let trails = input.trails.get(&history.repo);
        let mut emit = Emit {
            out: &mut out,
            at,
            repo: &history.repo,
        };
        for pr in history.prs.values() {
            let trail = trails
                .and_then(|trails| trails.get(&pr.number))
                .map_or(&[][..], Vec::as_slice);
            evaluate_pr(&mut emit, history, pr, trail, config, live);
        }
        for group in &history.group_runs {
            let Some(number) = group.pr else { continue };
            let Some(pr) = history.prs.get(&number) else {
                continue;
            };
            if !group.gate_failed() || group.settled_at() > at {
                continue;
            }
            let failed: Vec<&CheckFact> = group
                .required_jobs
                .iter()
                .filter(|job| job.failed())
                .collect();
            let tests: Vec<String> = failed
                .iter()
                .flat_map(|job| job.signatures.iter().cloned())
                .collect();
            let checks: Vec<&str> = failed.iter().map(|job| job.name.as_str()).collect();
            emit.push(
                "group_red",
                format!("group_red:{}:{}", history.repo, group.id),
                Some(group.settled_at()),
                pr,
                None,
                &format!(
                    "merge group run {} named for it failed {} ({})",
                    group.id,
                    checks.join(", "),
                    tests_text(&tests)
                ),
                json!({"run_id": group.id, "checks": checks, "tests": tests}),
                fix::group_red(&history.repo, pr, group.id, &tests),
            );
        }
    }
    for alarm in &input.alarms {
        if alarm.opened_at > at {
            continue;
        }
        out.push(RuleEvent {
            schema: EVENT_SCHEMA.to_owned(),
            kind: "host_alarm".to_owned(),
            key: format!("host_alarm:{}#{}", alarm.repo, alarm.number),
            at,
            since: Some(alarm.opened_at),
            repo: Some(alarm.repo.clone()),
            pr: None,
            head: None,
            summary: format!("{}#{}: {}", alarm.repo, alarm.number, alarm.title),
            detail: json!({"issue": alarm.number, "title": alarm.title,
                           "closed_at": alarm.closed_at.map(stamp)}),
            fix: fix::host_alarm(alarm),
        });
    }
    for event in &input.inputs {
        if event.at > at {
            continue;
        }
        let summary = event
            .rest
            .get("summary")
            .and_then(Value::as_str)
            .map_or_else(|| format!("{}: {}", event.kind, event.key), str::to_owned);
        out.push(RuleEvent {
            schema: EVENT_SCHEMA.to_owned(),
            kind: event.kind.clone(),
            key: format!("input:{}", event.key),
            at,
            since: Some(event.at),
            repo: event
                .rest
                .get("repo")
                .and_then(Value::as_str)
                .map(str::to_owned),
            pr: event.rest.get("pr").and_then(Value::as_u64),
            head: None,
            summary,
            detail: serde_json::to_value(&event.rest).unwrap_or(Value::Null),
            fix: fix::from_input(&event.kind, &event.key, &event.rest),
        });
    }
    out
}

fn evaluate_pr(
    emit: &mut Emit<'_>,
    history: &RepoHistory,
    pr: &PrHistory,
    trail: &[TrailEntry],
    config: &RuleConfig,
    live: bool,
) {
    // A verdict is routed whenever it was posted, open or not.
    verdict_events(emit, history, pr, trail);
    if !open_at(pr, emit.at) {
        return;
    }
    removal_events(emit, history, pr);
    let Some(head) = head_at(pr, emit.at) else {
        return;
    };
    let state = pr_state(pr, emit.at);
    let view = HeadView {
        history,
        pr,
        head,
        verdict: latest_verdict(trail, &head.sha, emit.at, pr.author.as_deref()),
        in_flight: state.armed || state.queued_since.is_some(),
    };
    review_events(emit, &view, trail, config);
    armed_events(emit, &view, &state, config);
    check_events(emit, &view);
    queue_events(emit, &view, &state, config);
    approval_events(emit, &view, config, live);
}

/// One open pull request's current head, with what the rules read about it.
struct HeadView<'a> {
    history: &'a RepoHistory,
    pr: &'a PrHistory,
    head: &'a super::HeadFact,
    verdict: Option<&'a TrailEntry>,
    in_flight: bool,
}

impl HeadView<'_> {
    fn sha(&self) -> &str {
        &self.head.sha
    }

    fn approved(&self) -> Option<&TrailEntry> {
        self.verdict.filter(|entry| {
            entry
                .verdict
                .as_ref()
                .is_some_and(|verdict| verdict.approve)
        })
    }

    fn key(&self, kind: &str) -> String {
        format!(
            "{kind}:{}#{}@{}",
            self.history.repo,
            self.pr.number,
            self.sha()
        )
    }
}

fn verdict_events(
    emit: &mut Emit<'_>,
    history: &RepoHistory,
    pr: &PrHistory,
    trail: &[TrailEntry],
) {
    let at = emit.at;
    let author = pr.author.as_deref();
    for entry in trail
        .iter()
        .filter(|entry| entry.at <= at && trusted(entry, author))
    {
        let Some(verdict) = &entry.verdict else {
            continue;
        };
        let word = if verdict.approve { "APPROVE" } else { "HOLD" };
        emit.push(
            "verdict_posted",
            format!(
                "verdict_posted:{}:{}:{}",
                history.repo,
                if entry.review { "review" } else { "comment" },
                entry.id
            ),
            Some(entry.at),
            pr,
            Some(&verdict.head),
            &format!(
                "{} posted {word} on head {}; route it to the author",
                entry.login,
                short(&verdict.head)
            ),
            json!({"verdict": word, "reviewer": entry.login, "verdict_head": verdict.head,
                   "author": pr.author}),
            fix::verdict(&history.repo, pr, entry, verdict.approve, &verdict.head),
        );
    }
}

fn removal_events(emit: &mut Emit<'_>, history: &RepoHistory, pr: &PrHistory) {
    let at = emit.at;
    for event in pr.events.iter().filter(|event| event.at <= at) {
        if let QueueEventKind::Removed { reason } = &event.kind
            && reason != "merged"
        {
            emit.push(
                "queue_removed",
                format!(
                    "queue_removed:{}#{}@{}",
                    history.repo,
                    pr.number,
                    stamp(event.at)
                ),
                Some(event.at),
                pr,
                None,
                &format!("the merge queue removed it ({reason})"),
                json!({"reason": reason}),
                fix::queue_removed(&history.repo, pr, reason),
            );
        }
    }
}

fn review_events(
    emit: &mut Emit<'_>,
    view: &HeadView<'_>,
    trail: &[TrailEntry],
    config: &RuleConfig,
) {
    if view.verdict.is_some() {
        return;
    }
    let sha = view.sha();
    let ready = trail
        .iter()
        .filter(|entry| entry.at <= emit.at)
        .filter(|entry| {
            entry
                .ready_head
                .as_deref()
                .is_some_and(|ready| ready.eq_ignore_ascii_case(sha))
        })
        .min_by_key(|entry| (entry.at, entry.id));
    if let Some(ready) = ready {
        emit.push(
            "ready_unreviewed",
            view.key("ready_unreviewed"),
            Some(ready.at),
            view.pr,
            Some(sha),
            &format!(
                "head {} is READY ({}) and no verdict names it; route it to a reviewer",
                short(sha),
                ready.login
            ),
            json!({"ready_by": ready.login, "ready_comment": ready.id}),
            fix::ready(
                &view.history.repo,
                view.pr,
                sha,
                ready,
                config.reviewer(&view.history.repo),
            ),
        );
    } else if let Some(wait) = config.head_unreviewed_after
        && !view.pr.draft
        && view.head.first_seen_at + wait <= emit.at
    {
        emit.push(
            "head_unreviewed",
            view.key("head_unreviewed"),
            Some(view.head.first_seen_at),
            view.pr,
            Some(sha),
            &format!(
                "head {} has been up since {} with no READY and no verdict",
                short(sha),
                stamp(view.head.first_seen_at)
            ),
            json!({"head_since": stamp(view.head.first_seen_at)}),
            fix::head_unreviewed(&view.history.repo, view.pr, sha),
        );
    }
}

fn armed_events(emit: &mut Emit<'_>, view: &HeadView<'_>, state: &PrState, config: &RuleConfig) {
    if !view.in_flight || view.approved().is_some() {
        return;
    }
    let what = if state.queued_since.is_some() {
        "queued"
    } else {
        "armed"
    };
    let latest = view.verdict.map_or_else(
        || "no verdict names it".to_owned(),
        |entry| format!("the latest verdict on it is HOLD by {}", entry.login),
    );
    emit.push(
        "armed_unapproved",
        view.key("armed_unapproved"),
        state.queued_since.or(state.armed_since),
        view.pr,
        Some(view.sha()),
        &format!("{what} at head {}, but {latest}", short(view.sha())),
        json!({"state": what}),
        fix::armed_unapproved(
            &view.history.repo,
            view.pr,
            view.sha(),
            config.reviewer(&view.history.repo),
        ),
    );
}

fn check_events(emit: &mut Emit<'_>, view: &HeadView<'_>) {
    // A red head nobody armed or approved is its author's work in progress.
    if !view.in_flight && view.approved().is_none() {
        return;
    }
    let sha = view.sha();
    for check in latest_checks(&view.head.checks, emit.at) {
        if !red(check) || check.completed_at.is_none_or(|done| done > emit.at) {
            continue;
        }
        let conclusion = check.conclusion.as_deref().unwrap_or_default();
        emit.push(
            "required_red",
            format!("{}:{}:{}", view.key("required_red"), check.name, check.id),
            check.completed_at,
            view.pr,
            Some(sha),
            &format!(
                "required check {} {conclusion} on head {}{} ({})",
                check.name,
                short(sha),
                if view.in_flight { ", armed" } else { "" },
                tests_text(&check.signatures)
            ),
            json!({"check": check.name, "check_run": check.id, "conclusion": conclusion,
                   "tests": check.signatures, "armed": view.in_flight}),
            fix::required_red(&view.history.repo, view.pr, sha, check),
        );
    }
}

fn queue_events(emit: &mut Emit<'_>, view: &HeadView<'_>, state: &PrState, config: &RuleConfig) {
    let Some(since) = state.queued_since else {
        return;
    };
    if since + config.queue_stuck_after > emit.at {
        return;
    }
    emit.push(
        "queue_stuck",
        format!(
            "queue_stuck:{}#{}@{}",
            view.history.repo,
            view.pr.number,
            stamp(since)
        ),
        Some(since),
        view.pr,
        Some(view.sha()),
        &format!("in the merge queue since {} and not merged", stamp(since)),
        json!({"queued_since": stamp(since)}),
        fix::queue_stuck(
            &view.history.repo,
            &view.history.base,
            view.pr,
            &stamp(since),
        ),
    );
}

fn approval_events(emit: &mut Emit<'_>, view: &HeadView<'_>, config: &RuleConfig, live: bool) {
    let Some(approval) = view.approved() else {
        return;
    };
    let sha = view.sha();
    if !view.in_flight && !view.pr.draft && approval.at + config.approved_unarmed_after <= emit.at {
        emit.push(
            "approved_unarmed",
            view.key("approved_unarmed"),
            Some(approval.at),
            view.pr,
            Some(sha),
            &format!(
                "head {} approved by {} and nothing armed or queued it",
                short(sha),
                approval.login
            ),
            json!({"approved_by": approval.login}),
            fix::approved_unarmed(&view.history.repo, view.pr, sha, approval),
        );
    }
    if live
        && let Some(merge_state) = view.pr.merge_state.as_deref()
        && matches!(merge_state, "DIRTY" | "BEHIND")
        && view.pr.head_sha.eq_ignore_ascii_case(sha)
    {
        emit.push(
            "approved_blocked",
            format!("{}:{merge_state}", view.key("approved_blocked")),
            Some(approval.at),
            view.pr,
            Some(sha),
            &format!("head {} is approved but {merge_state}", short(sha)),
            json!({"merge_state": merge_state}),
            fix::approved_blocked(&view.history.repo, view.pr, sha, merge_state),
        );
    }
}

/// Keys already emitted, so each condition fires once.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleLedger {
    /// [`LEDGER_SCHEMA`].
    pub schema: String,
    /// Key to when it was first seen.
    pub seen: BTreeMap<String, DateTime<Utc>>,
}

impl RuleLedger {
    /// An empty ledger.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema: LEDGER_SCHEMA.to_owned(),
            seen: BTreeMap::new(),
        }
    }

    /// Record every key without emitting: the state a new watcher starts
    /// from, so it reports changes rather than the whole backlog.
    pub fn prime(&mut self, events: &[RuleEvent], at: DateTime<Utc>) {
        for event in events {
            self.seen.entry(event.key.clone()).or_insert(at);
        }
    }

    /// The events whose key was never emitted, recording them.
    pub fn fold(&mut self, events: Vec<RuleEvent>) -> Vec<RuleEvent> {
        let mut fresh = Vec::new();
        let mut now = BTreeSet::new();
        for event in events {
            if self.seen.contains_key(&event.key) || !now.insert(event.key.clone()) {
                continue;
            }
            self.seen.insert(event.key.clone(), event.at);
            fresh.push(event);
        }
        fresh
    }

    /// Drop keys first seen more than [`LEDGER_KEEP_DAYS`] before `at`.
    pub fn prune(&mut self, at: DateTime<Utc>) {
        let floor = at - Duration::days(LEDGER_KEEP_DAYS);
        self.seen.retain(|_, seen| *seen >= floor);
    }
}

/// Default ledger path for one set of watched repositories.
#[must_use]
pub fn ledger_path(state_dir: &Path, repos: &[String]) -> PathBuf {
    let mut name = repos
        .iter()
        .map(|repo| repo.replace('/', "__"))
        .collect::<Vec<_>>()
        .join("+");
    if name.len() > 120 {
        name.truncate(120);
    }
    state_dir
        .join("pr-watch")
        .join(format!("rules-{name}.json"))
}

/// Read a ledger; `None` when the file does not exist.
///
/// # Errors
/// When it exists but cannot be read or parsed: an unreadable ledger is not
/// an empty one.
pub fn load(path: &Path) -> Result<Option<RuleLedger>, String> {
    match std::fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map(Some)
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Write a ledger atomically.
///
/// # Errors
/// When the file cannot be written.
pub fn save(path: &Path, ledger: &RuleLedger) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string_pretty(ledger).map_err(|error| error.to_string())?;
    std::fs::write(&tmp, body).map_err(|error| format!("{}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, path).map_err(|error| format!("{}: {error}", path.display()))
}

/// What one live pass did.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PassOutcome {
    /// The ledger did not exist, so this pass only recorded the current
    /// conditions.
    pub primed: bool,
    /// Events to emit.
    pub events: Vec<RuleEvent>,
}

/// One live pass over an existing (or absent) ledger.
#[must_use]
pub fn pass(
    input: &WatchInput,
    ledger: Option<RuleLedger>,
    now: DateTime<Utc>,
    config: &RuleConfig,
) -> (RuleLedger, PassOutcome) {
    let holding = evaluate(input, now, config, true);
    let Some(mut ledger) = ledger else {
        let mut ledger = RuleLedger::new();
        ledger.prime(&holding, now);
        return (
            ledger,
            PassOutcome {
                primed: true,
                events: Vec::new(),
            },
        );
    };
    let events = ledger.fold(holding);
    ledger.prune(now);
    (
        ledger,
        PassOutcome {
            primed: false,
            events,
        },
    )
}

/// Replay `from..=to` at `tick`: prime at `from`, then emit every key that
/// first holds at a later tick, stamped with that tick.
#[must_use]
pub fn replay(
    input: &WatchInput,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    tick: Duration,
    config: &RuleConfig,
) -> Vec<RuleEvent> {
    let mut ledger = RuleLedger::new();
    ledger.prime(&evaluate(input, from, config, false), from);
    let mut out = Vec::new();
    let mut at = from + tick;
    while at <= to {
        out.extend(ledger.fold(evaluate(input, at, config, false)));
        at += tick;
    }
    out
}

/// Count of events by kind, for a summary line.
#[must_use]
pub fn tally(events: &[RuleEvent]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for event in events {
        *counts.entry(event.kind.as_str()).or_default() += 1;
    }
    let mut text = String::new();
    for (kind, count) in counts {
        let _ = write!(
            text,
            "{}{kind}={count}",
            if text.is_empty() { "" } else { " " }
        );
    }
    text
}

#[cfg(test)]
mod tests;
