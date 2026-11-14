//! Routine release-movement acts, decided by policy rather than by whoever
//! holds a token: `shipyard advance`.
//!
//! Two acts used to go to the owner because the rule that made them
//! legitimate lived in comments and decision files, not in a tool:
//!
//! * **retrigger**: close and reopen a pull request so GitHub rebuilds a
//!   stale merge ref, when nothing about the head changed and nobody else is
//!   working on it;
//! * **release-hold**: remove the `shipyard:no-auto-merge` label once every
//!   condition the hold named is met.
//!
//! Everything here is pure: the caller reads the facts, these functions
//! decide, and the caller performs the one write the decision names. The
//! merge steward can call the same functions later to perform these acts
//! itself. A fact that could not be read is never passed in as empty; the
//! caller refuses instead, so partial data is never judged clean.
//!
//! Defect dequeues are not an act here: they already have one audited path,
//! the queue-removal guard's `defect-fix` reason (`docs/ghapp-guards.md`).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::{Value, json};

use crate::head_approval::{Approval, HeadGate};
use crate::merge_steward::{RequiredCheck, StewardCheck, selected_required_check};
use crate::owner_registry::OwnerRecord;

/// The label a hold adds and `release-hold` removes.
pub const HOLD_LABEL: &str = "shipyard:no-auto-merge";
/// A line in the hold comment naming one condition the hold waits on.
pub const HOLD_CONDITION_PREFIX: &str = "hold-condition:";
/// How far from the label event the hold comment may be posted.
pub const HOLD_COMMENT_WINDOW: Duration = Duration::minutes(15);
/// The local audit log, in Shipyard's state directory beside
/// `queue-removals.jsonl`.
pub const AUDIT_LOG: &str = "advance.jsonl";
/// Opening text of the marker every audit comment carries.
pub const AUDIT_MARKER: &str = "<!-- shipyard-advance:v1";
/// The shortest `--reason` accepted.
pub const MIN_REASON_CHARS: usize = 10;

/// Environment variables that name this process's verdict stamp session, in
/// the order the `ghapp` stamp hook reads them.
const STAMP_SESSION_ENVS: [&str; 3] = [
    "SHIPYARD_STAMP_SESSION_ID",
    "CLAUDE_CODE_SESSION_ID",
    "CODEX_COMPANION_SESSION_ID",
];
/// Further session ids that may have stamped a pull request this process
/// opened (`whence` and the owner registry read these).
const OTHER_SESSION_ENVS: [&str; 4] = [
    "WHENCE_SESSION_ID",
    "CODEX_SESSION_ID",
    "CODEX_THREAD_ID",
    "CODEX_ROLLOUT_ID",
];

/// One pull request, `owner/repo#number`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Target {
    /// Exact `owner/repo`.
    pub repo: String,
    /// Pull request number.
    pub pr: u64,
}

impl Target {
    /// Parse `owner/repo#number`.
    ///
    /// # Errors
    ///
    /// When the text is not exactly that shape.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (repo, number) = text
            .trim()
            .split_once('#')
            .ok_or_else(|| format!("{text:?} is not OWNER/REPO#NUMBER"))?;
        crate::gh::validate_repo_slug(repo)
            .map_err(|_| format!("{repo:?} is not an exact OWNER/REPO"))?;
        let pr = number
            .parse::<u64>()
            .ok()
            .filter(|pr| *pr > 0 && number.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| format!("{number:?} is not a pull request number"))?;
        Ok(Self {
            repo: repo.to_owned(),
            pr,
        })
    }

    /// `owner/repo#number`.
    #[must_use]
    pub fn label(&self) -> String {
        format!("{}#{}", self.repo, self.pr)
    }
}

/// Which act.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Act {
    /// Close and reopen to rebuild a stale merge ref.
    Retrigger,
    /// Remove [`HOLD_LABEL`].
    ReleaseHold,
}

impl Act {
    /// The act's name, as the CLI flag spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Retrigger => "retrigger",
            Self::ReleaseHold => "release-hold",
        }
    }
}

/// Who is acting, identified the way `ghapp` stamps verdicts
/// (`<!-- session: <host>/<session> agent: <id> -->`) and `shipyard pr`
/// records owners (`<!-- shipyard-owner: host=... sid=... agent=... -->`).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct Actor {
    /// Short host name.
    pub host: String,
    /// The session the `ghapp` stamp hook would stamp, when one is set.
    pub stamp_session: Option<String>,
    /// Every session id this process carries.
    pub sessions: BTreeSet<String>,
    /// `SHIPYARD_AGENT_ID`, when set.
    pub agent: Option<String>,
}

impl Actor {
    /// Read the actor from `env`; `host` is the short host name.
    #[must_use]
    pub fn from_env(host: String, env: &dyn Fn(&str) -> Option<String>) -> Self {
        let get = |name: &str| {
            env(name)
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty() && value != "unattributed")
        };
        let stamp_session = STAMP_SESSION_ENVS.iter().find_map(|name| get(name));
        let sessions = STAMP_SESSION_ENVS
            .iter()
            .chain(OTHER_SESSION_ENVS.iter())
            .filter_map(|name| get(name))
            .collect();
        Self {
            host,
            stamp_session,
            sessions,
            agent: get(crate::head_approval::AGENT_ENV),
        }
    }

    /// Whether anything identifies this actor. An actor nothing identifies
    /// cannot be shown not to be the author or an approver, so it is refused.
    #[must_use]
    pub fn identified(&self) -> bool {
        self.agent.is_some() || !self.sessions.is_empty()
    }

    /// `host=<h> sid=<s> agent=<a>`, the values that are set. Never spells
    /// the word the `ghapp` body guard refuses inside an HTML comment.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut parts = vec![format!("host={}", self.host)];
        if let Some(session) = &self.stamp_session {
            parts.push(format!("sid={session}"));
        }
        if let Some(agent) = &self.agent {
            parts.push(format!("agent={agent}"));
        }
        parts.join(" ")
    }

    fn owns_session(&self, session: Option<&str>) -> bool {
        session.is_some_and(|session| self.sessions.contains(session))
    }

    fn is_agent(&self, agent: Option<&str>) -> bool {
        matches!((self.agent.as_deref(), agent), (Some(mine), Some(theirs)) if mine == theirs)
    }

    /// Whether a `ghapp` stamp in `body` names this actor.
    fn stamped(&self, body: &str) -> bool {
        self.is_agent(crate::head_approval::stamped_agent(body).as_deref())
            || self.owns_session(crate::head_approval::stamped_session(body).as_deref())
    }

    /// Whether an owner record names this actor.
    fn owns(&self, record: &OwnerRecord) -> bool {
        self.is_agent(record.agent.as_deref()) || self.owns_session(record.session.as_deref())
    }
}

/// The pull request as read from `repos/<repo>/pulls/<n>`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct PrFacts {
    /// `open` or `closed`.
    pub state: String,
    /// Whether it merged.
    pub merged: bool,
    /// Full head sha.
    pub head: String,
    /// Base branch.
    pub base: String,
    /// Author login.
    pub author: String,
    /// Body, for the owner and `whence` markers.
    pub body: String,
    /// Label names.
    pub labels: Vec<String>,
}

impl PrFacts {
    /// Parse a REST pull request.
    ///
    /// # Errors
    ///
    /// When any field the decisions need is missing or malformed.
    pub fn parse(value: &Value) -> Result<Self, String> {
        let text = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("pull request has no {pointer}"))
        };
        let head = text("/head/sha")?.to_ascii_lowercase();
        if !is_full_sha(&head) {
            return Err(format!("pull request head {head:?} is not a full sha"));
        }
        let labels = value
            .get("labels")
            .and_then(Value::as_array)
            .ok_or_else(|| "pull request has no labels array".to_owned())?
            .iter()
            .map(|label| {
                label
                    .get("name")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| "pull request label has no name".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            state: text("/state")?,
            merged: value
                .get("merged")
                .and_then(Value::as_bool)
                .ok_or_else(|| "pull request has no merged flag".to_owned())?,
            head,
            base: text("/base/ref")?,
            author: text("/user/login")?,
            body: value
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            labels,
        })
    }
}

/// Merge-queue membership from a `PR_QUEUE_STATE_QUERY` answer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct QueueFacts {
    /// The head GraphQL saw.
    pub head: String,
    /// Whether it is in the merge queue.
    pub in_queue: bool,
    /// Whether native auto-merge is armed.
    pub auto_merge_armed: bool,
}

impl QueueFacts {
    /// Parse the answer, refusing GraphQL errors and a missing pull request.
    ///
    /// # Errors
    ///
    /// When the answer does not say whether the pull request is queued.
    pub fn parse(value: &Value) -> Result<Self, String> {
        if let Some(errors) = value.get("errors").filter(|errors| !errors.is_null()) {
            return Err(format!("merge-queue state carried errors: {errors}"));
        }
        let pr = value
            .pointer("/data/repository/pullRequest")
            .filter(|pr| pr.is_object())
            .ok_or_else(|| "merge-queue state carried no pull request".to_owned())?;
        let head = pr
            .get("headRefOid")
            .and_then(Value::as_str)
            .map(str::to_ascii_lowercase)
            .filter(|head| is_full_sha(head))
            .ok_or_else(|| "merge-queue state carried no head".to_owned())?;
        let in_queue = pr
            .get("isInMergeQueue")
            .and_then(Value::as_bool)
            .ok_or_else(|| "merge-queue state carried no isInMergeQueue".to_owned())?;
        // Both fields must be present: an omitted one is an unread state,
        // never an empty queue entry or an unarmed pull request.
        let entry = !pr
            .get("mergeQueueEntry")
            .ok_or_else(|| "merge-queue state carried no mergeQueueEntry".to_owned())?
            .is_null();
        let auto_merge_armed = !pr
            .get("autoMergeRequest")
            .ok_or_else(|| "merge-queue state carried no autoMergeRequest".to_owned())?
            .is_null();
        Ok(Self {
            head,
            in_queue: in_queue || entry,
            auto_merge_armed,
        })
    }
}

/// Required checks at the exact head: the rule set from branch protection
/// and rulesets, and every check run and commit status read at that head.
#[derive(Clone, Debug, Default)]
pub struct CheckFacts {
    /// The required rules.
    pub required: Vec<RequiredCheck>,
    /// Every observation at the head.
    pub checks: Vec<StewardCheck>,
    /// Set when classic branch protection could not be read for a reason
    /// the steward also reads as "rulesets only" (a 403 for a credential
    /// without administration access): the rule set then comes from the
    /// rulesets alone, and the evidence says so rather than implying more.
    pub rulesets_only: Option<String>,
}

impl CheckFacts {
    fn source(&self) -> String {
        self.rulesets_only.as_ref().map_or_else(String::new, |why| {
            format!(" (required set from rulesets only; classic protection unread: {why})")
        })
    }
}

/// One issue event on the pull request (`repos/<repo>/issues/<n>/events`).
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct LabelEvent {
    /// `labeled` or `unlabeled`.
    pub event: String,
    /// Label name.
    pub label: String,
    /// Who did it.
    pub login: String,
    /// When.
    pub at: DateTime<Utc>,
}

impl LabelEvent {
    /// Parse one `{event, label, login, at}` record; `None` for an event
    /// that is not about a label.
    ///
    /// # Errors
    ///
    /// When a label event lacks a field.
    pub fn parse(value: &Value) -> Result<Option<Self>, String> {
        let event = value.get("event").and_then(Value::as_str).unwrap_or("");
        if event != "labeled" && event != "unlabeled" {
            return Ok(None);
        }
        let text = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("{event} event has no {key}"))
        };
        Ok(Some(Self {
            event: event.to_owned(),
            label: text("label")?,
            login: text("login")?,
            at: parse_time(&text("at")?)
                .ok_or_else(|| format!("{event} event has an unreadable time"))?,
        }))
    }
}

/// One condition a hold comment names, as a `hold-condition:` line.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HoldCondition {
    /// `hold-condition: merged owner/repo#n`: that pull request merged.
    Merged {
        /// The pull request.
        target: String,
    },
    /// `hold-condition: after <RFC 3339 time>`: that time passed.
    After {
        /// The time.
        at: String,
    },
    /// `hold-condition: approved`: an exact-head approval exists.
    Approved,
    /// `hold-condition: green`: every required check is green.
    Green,
    /// Anything else. It cannot be checked, so it is never met.
    Unchecked {
        /// The line's text.
        text: String,
    },
}

/// Every `hold-condition:` line in `body`.
#[must_use]
pub fn hold_conditions(body: &str) -> Vec<HoldCondition> {
    body.lines()
        .filter_map(|line| line.trim().strip_prefix(HOLD_CONDITION_PREFIX))
        .map(|text| {
            let text = text.trim();
            let mut words = text.split_whitespace();
            match (words.next(), words.next(), words.next()) {
                (Some("merged"), Some(target), None) if Target::parse(target).is_ok() => {
                    HoldCondition::Merged {
                        target: target.to_owned(),
                    }
                }
                (Some("after"), Some(at), None) if parse_time(at).is_some() => {
                    HoldCondition::After { at: at.to_owned() }
                }
                (Some("approved"), None, None) => HoldCondition::Approved,
                (Some("green"), None, None) => HoldCondition::Green,
                _ => HoldCondition::Unchecked {
                    text: text.to_owned(),
                },
            }
        })
        .collect()
}

/// A decision-file reference: `owner/repo:path[#anchor]`, or a GitHub blob
/// URL. The file's name must start with `DECISIONS`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DecisionRef {
    /// Repository holding the file.
    pub repo: String,
    /// Path inside it.
    pub path: String,
    /// Git ref, when the URL named one.
    pub git_ref: Option<String>,
    /// The text as given.
    pub text: String,
}

impl DecisionRef {
    /// Parse a reference.
    ///
    /// # Errors
    ///
    /// When it is not one of the two shapes, or names a file that is not a
    /// decision file.
    pub fn parse(text: &str) -> Result<Self, String> {
        let trimmed = text.trim();
        let without_anchor = trimmed.split('#').next().unwrap_or_default();
        let (repo, path, git_ref) =
            if let Some(rest) = without_anchor.strip_prefix("https://github.com/") {
                let parts: Vec<&str> = rest.splitn(5, '/').collect();
                match parts.as_slice() {
                    [owner, name, "blob", git_ref, path] => (
                        format!("{owner}/{name}"),
                        (*path).to_owned(),
                        Some((*git_ref).to_owned()),
                    ),
                    _ => return Err(format!("{trimmed:?} is not a GitHub blob URL")),
                }
            } else {
                let (repo, path) = without_anchor
                    .split_once(':')
                    .ok_or_else(|| format!("{trimmed:?} is not OWNER/REPO:PATH or a blob URL"))?;
                (repo.to_owned(), path.to_owned(), None)
            };
        crate::gh::validate_repo_slug(&repo)
            .map_err(|_| format!("{repo:?} is not an exact OWNER/REPO"))?;
        let name = path.rsplit('/').next().unwrap_or_default();
        if path.is_empty()
            || path.split('/').any(|part| part.is_empty() || part == "..")
            || !name.starts_with("DECISIONS")
        {
            return Err(format!(
                "{trimmed:?} does not name a DECISIONS file (its file name must start with \
                 DECISIONS)"
            ));
        }
        Ok(Self {
            repo,
            path,
            git_ref,
            text: trimmed.to_owned(),
        })
    }
}

/// Whether decision-file `content` names `target` (`owner/repo#n`, or its
/// pull-request URL), so a decision recorded for one pull request cannot
/// release another.
#[must_use]
pub fn decision_names(content: &str, target: &Target) -> bool {
    let short = format!(r"(?i)\b{}#{}\b", regex::escape(&target.repo), target.pr);
    let url = format!(
        r"(?i)github\.com/{}/pull/{}\b",
        regex::escape(&target.repo),
        target.pr
    );
    [short, url]
        .iter()
        .any(|pattern| Regex::new(pattern).is_ok_and(|pattern| pattern.is_match(content)))
}

/// Why an act is refused: the condition that failed, and what was seen.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Refusal {
    /// A stable token naming the failed condition.
    pub condition: &'static str,
    /// What was seen, in words.
    pub detail: String,
}

impl Refusal {
    fn new(condition: &'static str, detail: impl Into<String>) -> Self {
        Self {
            condition,
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.condition, self.detail)
    }
}

/// An allowed act, bound to the exact head it was judged at.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Decision {
    /// What to do.
    pub act: Act,
    /// Where.
    pub target: Target,
    /// The head every fact was read at; the act refuses if it moved.
    pub head: String,
    /// Why, as the caller gave it (or the decision reference).
    pub reason: String,
    /// Each condition checked, with what satisfied it.
    pub evidence: Vec<String>,
}

/// What the retrigger decision needs.
#[derive(Clone, Debug)]
pub struct RetriggerFacts {
    /// The pull request.
    pub pr: PrFacts,
    /// Its merge-queue state.
    pub queue: QueueFacts,
    /// Required checks at the head.
    pub checks: CheckFacts,
    /// Every issue comment, as `{id, login, type, association, at, body}`.
    pub comments: Vec<Value>,
    /// This host's owner record for the pull request, when it has one.
    pub owner_record: Option<OwnerRecord>,
}

/// What the release-hold decision needs.
#[derive(Clone, Debug)]
pub struct ReleaseHoldFacts {
    /// The pull request.
    pub pr: PrFacts,
    /// Required checks at the head.
    pub checks: CheckFacts,
    /// The approval gate at the head, from the steward's own evaluation.
    pub gate: HeadGate,
    /// Every issue comment, as `{id, login, type, association, at, body}`.
    pub comments: Vec<Value>,
    /// Every label event, oldest first.
    pub events: Vec<LabelEvent>,
    /// For each `merged` condition target, whether it merged.
    pub merged: BTreeMap<String, bool>,
    /// The decision reference and its file's content, when one was given.
    pub decision: Option<(DecisionRef, String)>,
    /// This host's owner record for the pull request, when it has one.
    pub owner_record: Option<OwnerRecord>,
}

/// Decide a retrigger.
///
/// `expected_head`, when given, is the head the caller meant; a different
/// live head refuses.
///
/// # Errors
///
/// The first condition that fails.
pub fn decide_retrigger(
    target: &Target,
    facts: &RetriggerFacts,
    actor: &Actor,
    reason: &str,
    expected_head: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Decision, Refusal> {
    let mut evidence = common(
        target,
        &facts.pr,
        actor,
        expected_head,
        &facts.comments,
        facts.owner_record.as_ref(),
    )?;
    let reason = reason.trim();
    if reason.chars().count() < MIN_REASON_CHARS {
        return Err(Refusal::new(
            "reason_missing",
            format!("--reason must say why, in at least {MIN_REASON_CHARS} characters"),
        ));
    }
    if facts.queue.head != facts.pr.head {
        return Err(Refusal::new(
            "head_moved",
            format!(
                "the pull request reads head {} but its merge-queue state reads {}; the head \
                 moved between reads",
                facts.pr.head, facts.queue.head
            ),
        ));
    }
    if facts.queue.in_queue {
        return Err(Refusal::new(
            "queued",
            "the pull request is in the merge queue; closing it would drop its entry and \
             discard the merge-group run",
        ));
    }
    if facts.queue.auto_merge_armed {
        return Err(Refusal::new(
            "armed",
            "native auto-merge is armed; closing the pull request would disarm it",
        ));
    }
    evidence.push("not in the merge queue and auto-merge not armed".to_owned());
    let live = live_repair_claims(&facts.comments, now);
    if !live.is_empty() {
        return Err(Refusal::new(
            "repair_claim_live",
            format!("a repair lane holds the pull request: {}", live.join("; ")),
        ));
    }
    evidence.push("no live repair claim".to_owned());
    evidence.push(head_unchanged_since_required_run(
        &facts.pr.head,
        &facts.checks,
    )?);
    Ok(Decision {
        act: Act::Retrigger,
        target: target.clone(),
        head: facts.pr.head.clone(),
        reason: reason.to_owned(),
        evidence,
    })
}

/// Decide a release-hold.
///
/// # Errors
///
/// The first condition that fails.
pub fn decide_release_hold(
    target: &Target,
    facts: &ReleaseHoldFacts,
    actor: &Actor,
    expected_head: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Decision, Refusal> {
    let mut evidence = common(
        target,
        &facts.pr,
        actor,
        expected_head,
        &facts.comments,
        facts.owner_record.as_ref(),
    )?;
    if !facts.pr.labels.iter().any(|label| label == HOLD_LABEL) {
        return Err(Refusal::new(
            "hold_label_absent",
            format!("the pull request does not carry {HOLD_LABEL}; there is no hold to release"),
        ));
    }
    evidence.push(required_green(&facts.pr.head, &facts.checks)?);
    evidence.push(counted_approval(
        &facts.pr.head,
        &facts.gate,
        &facts.comments,
    )?);
    let reason = if let Some((reference, content)) = &facts.decision {
        if !decision_names(content, target) {
            return Err(Refusal::new(
                "decision_unrelated",
                format!(
                    "{} does not name {} (as {} or its pull-request URL)",
                    reference.text,
                    target.label(),
                    target.label()
                ),
            ));
        }
        evidence.push(format!(
            "hold conditions replaced by decision {}, which names {}",
            reference.text,
            target.label()
        ));
        format!("decision {}", reference.text)
    } else {
        let comment = hold_comment(&facts.events, &facts.comments)?;
        let id = comment
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let body = comment.get("body").and_then(Value::as_str).unwrap_or("");
        let conditions = hold_conditions(body);
        if conditions.is_empty() {
            return Err(Refusal::new(
                "hold_conditions_unnamed",
                format!(
                    "hold comment {id} names no `{HOLD_CONDITION_PREFIX}` line, so its \
                     conditions cannot be checked; pass --decision with the DECISIONS entry that \
                     releases it"
                ),
            ));
        }
        for condition in &conditions {
            evidence.push(condition_met(condition, &facts.merged, now)?);
        }
        format!("every condition hold comment {id} names is met")
    };
    Ok(Decision {
        act: Act::ReleaseHold,
        target: target.clone(),
        head: facts.pr.head.clone(),
        reason,
        evidence,
    })
}

/// The checks both acts share: an identified actor, an open pull request at
/// the expected head, and an actor that neither authored nor approved it.
fn common(
    target: &Target,
    pr: &PrFacts,
    actor: &Actor,
    expected_head: Option<&str>,
    comments: &[Value],
    owner_record: Option<&OwnerRecord>,
) -> Result<Vec<String>, Refusal> {
    if !actor.identified() {
        return Err(Refusal::new(
            "actor_unidentified",
            "no session id or SHIPYARD_AGENT_ID identifies the caller, so it cannot be shown not \
             to be the author or an approver",
        ));
    }
    if pr.state != "open" || pr.merged {
        return Err(Refusal::new(
            "not_open",
            format!(
                "{} is {}",
                target.label(),
                if pr.merged { "merged" } else { "closed" }
            ),
        ));
    }
    if let Some(expected) = expected_head
        && !expected.eq_ignore_ascii_case(&pr.head)
    {
        return Err(Refusal::new(
            "head_moved",
            format!("expected head {expected}, but the live head is {}", pr.head),
        ));
    }
    let mut evidence = vec![format!("open at head {}", pr.head)];
    if let Some(found) = authored_by(actor, target, pr, owner_record) {
        return Err(Refusal::new(
            "actor_authored",
            format!(
                "the caller ({}) authored this pull request: {found}",
                actor.describe()
            ),
        ));
    }
    if let Some(id) = approved_by(actor, comments) {
        return Err(Refusal::new(
            "actor_approved",
            format!(
                "the caller ({}) posted the approval in comment {id}; an approver does not \
                 move its own approval along",
                actor.describe()
            ),
        ));
    }
    evidence.push(format!(
        "the caller ({}) neither authored nor approved it",
        actor.describe()
    ));
    Ok(evidence)
}

/// Where the actor shows up as the pull request's author, if anywhere.
fn authored_by(
    actor: &Actor,
    target: &Target,
    pr: &PrFacts,
    owner_record: Option<&OwnerRecord>,
) -> Option<String> {
    if let Some(marker) = crate::owner_registry::parse_marker(&target.repo, target.pr, &pr.body)
        && actor.owns(&marker)
    {
        return Some("the body's shipyard-owner marker names it".to_owned());
    }
    if actor.owns_session(crate::pr_fold::whence_session(&pr.body).as_deref()) {
        return Some("the body's whence provenance names it".to_owned());
    }
    if owner_record.is_some_and(|record| actor.owns(record)) {
        return Some("this host's owner record names it".to_owned());
    }
    None
}

/// The id of an APPROVE verdict comment (any head) stamped with the actor.
fn approved_by(actor: &Actor, comments: &[Value]) -> Option<u64> {
    comments.iter().find_map(|comment| {
        let body = comment.get("body").and_then(Value::as_str)?;
        let (approve, _) = crate::head_approval::verdict_line(body)?;
        (approve && actor.stamped(body)).then(|| {
            comment
                .get("id")
                .and_then(Value::as_u64)
                .unwrap_or_default()
        })
    })
}

/// Every live `repair-claim: <lane> until <time>` from a trusted author. A
/// claim whose time does not parse is live: it cannot be shown expired.
#[must_use]
pub fn live_repair_claims(comments: &[Value], now: DateTime<Utc>) -> Vec<String> {
    let pattern = Regex::new(r"(?m)^\s*repair-claim:\s*(\S+)\s+until\s+(\S+)").expect("regex");
    comments
        .iter()
        .filter(|comment| trusted(comment))
        .flat_map(|comment| {
            let id = comment
                .get("id")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let body = comment.get("body").and_then(Value::as_str).unwrap_or("");
            pattern
                .captures_iter(body)
                .filter(|claim| parse_time(&claim[2]).is_none_or(|until| until > now))
                .map(|claim| format!("{} until {} (comment {id})", &claim[1], &claim[2]))
                .collect::<Vec<_>>()
        })
        .collect()
}

fn trusted(comment: &Value) -> bool {
    comment
        .get("association")
        .and_then(Value::as_str)
        .is_some_and(|association| {
            crate::head_approval::TRUSTED_ASSOCIATIONS.contains(&association)
        })
        || comment.get("type").and_then(Value::as_str) == Some("Bot")
}

/// The head is unchanged since the last required run: required runs are
/// reported at this exact head, and none is still running (a retrigger
/// would discard it).
fn head_unchanged_since_required_run(head: &str, checks: &CheckFacts) -> Result<String, Refusal> {
    if checks.required.is_empty() {
        return Err(Refusal::new(
            "required_checks_unknown",
            "the base names no required check, so there is no required run to compare the head \
             with",
        ));
    }
    let selected: Vec<(&RequiredCheck, &StewardCheck)> = checks
        .required
        .iter()
        .filter_map(|rule| selected_required_check(&checks.checks, rule).map(|check| (rule, check)))
        .collect();
    if selected.is_empty() {
        return Err(Refusal::new(
            "no_required_run_at_head",
            format!(
                "no required check has run at head {head}: the head moved since the last \
                 required run, or none ran; a retrigger does not apply"
            ),
        ));
    }
    let running: Vec<String> = selected
        .iter()
        .filter(|(_, check)| !check.status.eq_ignore_ascii_case("COMPLETED"))
        .map(|(rule, _)| rule.label())
        .collect();
    if !running.is_empty() {
        return Err(Refusal::new(
            "required_run_in_progress",
            format!(
                "required checks still running at head {head}: {}",
                running.join(", ")
            ),
        ));
    }
    Ok(format!(
        "head unchanged since the last required run: {} of {} required checks reported at \
         head {head}, none running",
        selected.len(),
        checks.required.len(),
    ) + &checks.source())
}

/// Every required check is green at the head.
fn required_green(head: &str, checks: &CheckFacts) -> Result<String, Refusal> {
    // Refused before anything else: with no required rule, "every required
    // check is green" proves nothing, whatever the shared helper answers.
    if checks.required.is_empty() {
        return Err(Refusal::new(
            "required_checks_unknown",
            "the base names no required check, so green cannot be shown",
        ));
    }
    if crate::merge_steward::required_checks_green(&checks.checks, &checks.required) {
        return Ok(format!(
            "all {} required checks green at head {head}",
            checks.required.len()
        ) + &checks.source());
    }
    let not_green: Vec<String> = checks
        .required
        .iter()
        .filter_map(|rule| {
            let state = selected_required_check(&checks.checks, rule).map_or_else(
                || "missing".to_owned(),
                |check| {
                    check
                        .conclusion
                        .clone()
                        .unwrap_or_else(|| check.status.clone())
                        .to_ascii_lowercase()
                },
            );
            (!matches!(state.as_str(), "success" | "neutral" | "skipped"))
                .then(|| format!("{} ({state})", rule.label()))
        })
        .collect();
    Err(Refusal::new(
        "required_not_green",
        format!("not green at head {head}: {}", not_green.join(", ")),
    ))
}

/// The steward's gate approves the head with an APPROVE of this exact head
/// from a counted reviewer: an approving review on the commit, or a
/// `ghapp`-stamped verdict comment. A carried, rollup or admission-check
/// approval is not an APPROVE of this head.
fn counted_approval(head: &str, gate: &HeadGate, comments: &[Value]) -> Result<String, Refusal> {
    let HeadGate::Approved { approval } = gate else {
        return Err(Refusal::new(
            "approval_missing",
            match gate {
                HeadGate::NotRequired => format!(
                    "the base does not require head approval, so no counted reviewer approves \
                     head {head}"
                ),
                other => other.explain(),
            },
        ));
    };
    match approval {
        Approval::Review { login, .. } => Ok(format!(
            "an APPROVED review by {login} on exact head {head}, counted by the steward's gate"
        )),
        Approval::Marker {
            login,
            comment_id: Some(id),
            ..
        } => {
            let body = comments
                .iter()
                .find(|comment| comment.get("id").and_then(Value::as_u64) == Some(*id))
                .and_then(|comment| comment.get("body").and_then(Value::as_str));
            match body.and_then(crate::head_approval::stamped_session) {
                Some(stamp) => Ok(format!(
                    "an APPROVE of exact head {head} by {login} in comment {id} (stamped \
                     {stamp}), counted by the steward's gate"
                )),
                None => Err(Refusal::new(
                    "approval_unstamped",
                    format!(
                        "the approval of head {head} in comment {id} carries no ghapp stamp, so \
                         it cannot be tied to a reviewer run"
                    ),
                )),
            }
        }
        Approval::Marker { .. } => Err(Refusal::new(
            "approval_unstamped",
            format!("the approval of head {head} names no comment to check its stamp on"),
        )),
        other => Err(Refusal::new(
            "approval_not_exact_head",
            format!(
                "head {head} carries {}, not an APPROVE of this exact head",
                other.describe()
            ),
        )),
    }
}

/// The comment that added the hold: the one comment by the login that last
/// added [`HOLD_LABEL`], posted within [`HOLD_COMMENT_WINDOW`] of it, that
/// names the label.
fn hold_comment<'a>(events: &[LabelEvent], comments: &'a [Value]) -> Result<&'a Value, Refusal> {
    let labeled = events
        .iter()
        .rev()
        .find(|event| event.label == HOLD_LABEL)
        .filter(|event| event.event == "labeled")
        .ok_or_else(|| {
            Refusal::new(
                "hold_event_unread",
                format!(
                    "no event that added {HOLD_LABEL} is on record after its last removal; the \
                     hold comment cannot be found"
                ),
            )
        })?;
    let candidates: Vec<&Value> = comments
        .iter()
        .filter(|comment| {
            comment.get("login").and_then(Value::as_str) == Some(labeled.login.as_str())
                && comment
                    .get("at")
                    .and_then(Value::as_str)
                    .and_then(parse_time)
                    .is_some_and(|at| (at - labeled.at).abs() <= HOLD_COMMENT_WINDOW)
                && comment
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| body.contains(HOLD_LABEL))
        })
        .collect();
    match candidates.as_slice() {
        [comment] => Ok(comment),
        [] => Err(Refusal::new(
            "hold_comment_missing",
            format!(
                "no comment by {} within {} minutes of adding {HOLD_LABEL} at {} names the \
                 label; pass --decision with the DECISIONS entry that releases it",
                labeled.login,
                HOLD_COMMENT_WINDOW.num_minutes(),
                labeled.at.to_rfc3339()
            ),
        )),
        many => Err(Refusal::new(
            "hold_comment_ambiguous",
            format!(
                "{} comments by {} near the label event name {HOLD_LABEL} ({}); pass --decision",
                many.len(),
                labeled.login,
                many.iter()
                    .map(|comment| comment
                        .get("id")
                        .and_then(Value::as_u64)
                        .unwrap_or_default()
                        .to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

/// Whether one condition is met. `approved` and `green` were already proven
/// before conditions are read.
fn condition_met(
    condition: &HoldCondition,
    merged: &BTreeMap<String, bool>,
    now: DateTime<Utc>,
) -> Result<String, Refusal> {
    match condition {
        HoldCondition::Approved => Ok("condition `approved` met".to_owned()),
        HoldCondition::Green => Ok("condition `green` met".to_owned()),
        HoldCondition::After { at } => match parse_time(at) {
            Some(when) if when <= now => Ok(format!("condition `after {at}` met")),
            _ => Err(Refusal::new(
                "hold_condition_unmet",
                format!("condition `after {at}` is not yet met"),
            )),
        },
        HoldCondition::Merged { target } => match merged.get(target) {
            Some(true) => Ok(format!("condition `merged {target}` met")),
            Some(false) => Err(Refusal::new(
                "hold_condition_unmet",
                format!("condition `merged {target}` is not met: it has not merged"),
            )),
            None => Err(Refusal::new(
                "hold_condition_unread",
                format!("condition `merged {target}` was not read"),
            )),
        },
        HoldCondition::Unchecked { text } => Err(Refusal::new(
            "hold_condition_unchecked",
            format!(
                "condition `{text}` is not one shipyard advance can check (merged, after, \
                 approved, green); pass --decision with the DECISIONS entry that releases it"
            ),
        )),
    }
}

/// The audit comment for an act.
#[must_use]
pub fn audit_comment(decision: &Decision, actor: &Actor, outcome: &str) -> String {
    let mut lines = vec![
        format!(
            "shipyard advance {}: {}",
            decision.act.as_str(),
            match decision.act {
                Act::Retrigger => "closed and reopened to rebuild the merge ref",
                Act::ReleaseHold => "removed the shipyard:no-auto-merge label",
            }
        ),
        String::new(),
        format!("- outcome: {outcome}"),
        format!("- head: {}", decision.head),
        format!("- why: {}", single_line(&decision.reason)),
        format!("- actor: {}", actor.describe()),
        "- evidence checked:".to_owned(),
    ];
    lines.extend(
        decision
            .evidence
            .iter()
            .map(|line| format!("  - {}", single_line(line))),
    );
    lines.push(String::new());
    lines.push(format!(
        "{AUDIT_MARKER} act={} head={} -->",
        decision.act.as_str(),
        decision.head
    ));
    lines.join("\n")
}

/// The audit log line for an act.
#[must_use]
pub fn audit_record(
    decision: &Decision,
    actor: &Actor,
    outcome: &str,
    comment: &Result<u64, String>,
    now: DateTime<Utc>,
) -> Value {
    json!({
        "at": now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "act": decision.act.as_str(),
        "repo": decision.target.repo,
        "pr": decision.target.pr,
        "head": decision.head,
        "reason": decision.reason,
        "evidence": decision.evidence,
        "outcome": outcome,
        "actor": actor,
        "audit_comment_id": comment.as_ref().ok(),
        "audit_comment_error": comment.as_ref().err(),
        "writer": Value::Object(crate::head_approval::writer_identity()),
    })
}

/// Text that cannot read as a verdict or a stamp: newlines folded, and the
/// verdict, `reviewed:` and stamp spellings broken.
fn single_line(text: &str) -> String {
    let folded = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let pattern =
        Regex::new(r"(?i)(verdict|reviewed|session|post-merge)(\s*):").expect("pattern compiles");
    pattern
        .replace_all(&folded, "$1$2 -")
        .replace("<!--", "< !--")
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text.trim())
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn is_full_sha(text: &str) -> bool {
    text.len() == 40 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests;
