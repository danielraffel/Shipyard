//! PR event relay: tell the session that owns a pull request when something
//! it is waiting on changed, and what to do next. Model-free.
//!
//! One pass, per repository:
//!
//! 1. Learn the owners: this host's [`crate::owner_registry`] store, and the
//!    `shipyard-owner:` body marker of every open pull request that carries
//!    one (read in the same GraphQL query, so a record written on another
//!    host is seen).
//! 2. Read every owned pull request in **one** batched GraphQL query
//!    ([`fetch`]): state, merge state, merge-queue membership, head and base,
//!    the newest comments (verdicts, repair claims), and the required checks.
//!    No REST: the shared App's REST budget is the fleet's scarcest resource.
//!    The pass counts its reads with the steward's [`crate::gh_pass::GhPass`]
//!    and, when the steward's tick report for the repository is fresh, reuses
//!    its per-PR `next_action` as the next step instead of deriving one.
//! 3. Fold each answer into a [`PrDigest`] and compare it with the last one
//!    ([`derive_events`]): `merged`, `closed`, `ejected`, `enqueued`,
//!    `verdict`, `required_check`, `dirty_after_base_move`, `claim_taken`,
//!    `stacked_base_merged`.
//! 4. Deliver each new event once ([`RelayEvent::key`] is the idempotency
//!    key), batched at most one message per pull request per rate-limit window
//!    (terminal events skip the wait), over [`transport`]: the owner's cmux
//!    surface, else the repository's lead, else its repair lane, else one PR
//!    comment. Every delivery, and the owner's next action after it, is
//!    appended to the delivery log.
//!
//! A message is refused, never sent, if it contains anything a guard could
//! read as a verdict ([`refuses_verdict_text`]): the relay never approves.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) mod config;
pub(crate) mod pass;
pub(crate) mod transport;

#[cfg(test)]
mod tests;

/// Prefix of every relayed line, and of the fallback PR comment.
pub(crate) const MESSAGE_PREFIX: &str = "shipyard-relay:";

/// Checks whose red means "no verdict yet", not something the author fixes.
pub(crate) const DEFAULT_REVIEW_GATES: [&str; 3] = [
    "review-verdict",
    "post-review-verdict",
    "release-path-review",
];

/// Aggregate of the required checks at a head.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub(crate) enum Checks {
    /// Every required check passed.
    Success,
    /// At least one required check failed.
    Failure,
    /// Some are still running, or none reported yet.
    Pending,
}

/// A repair lane's claim on a pull request (`repair-claim: <lane> until <t>`).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct Claim {
    pub(crate) by: String,
    pub(crate) until: String,
}

/// The facts the relay compares between passes for one pull request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct PrDigest {
    /// `OPEN`, `MERGED`, `CLOSED`, or `MISSING`.
    pub(crate) state: String,
    /// GitHub `mergeStateStatus` (`CLEAN`, `DIRTY`, `BEHIND`, ...).
    pub(crate) merge: String,
    pub(crate) head: String,
    pub(crate) base: String,
    #[serde(default)]
    pub(crate) base_oid: String,
    #[serde(default)]
    pub(crate) default_branch: String,
    #[serde(default)]
    pub(crate) in_queue: bool,
    pub(crate) checks: Checks,
    #[serde(default)]
    pub(crate) red: Vec<String>,
    /// `APPROVE` or `HOLD`, only when the verdict names the current head.
    #[serde(default)]
    pub(crate) verdict: Option<String>,
    #[serde(default)]
    pub(crate) claim: Option<Claim>,
    /// The merged pull request whose head was this one's base.
    #[serde(default)]
    pub(crate) stacked_base_merged: Option<u64>,
    /// Newest comment not written by the relay, for "did the owner act".
    #[serde(default)]
    pub(crate) last_comment_at: Option<String>,
    /// GraphQL node id, for the comment fallback.
    #[serde(default)]
    pub(crate) node_id: String,
}

impl PrDigest {
    pub(crate) fn terminal(&self) -> bool {
        self.state == "MERGED" || self.state == "CLOSED"
    }

    pub(crate) fn short_head(&self) -> &str {
        self.head.get(..10).unwrap_or(&self.head)
    }
}

/// One thing that happened to a pull request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum RelayEvent {
    Merged {
        head: String,
    },
    Closed {
        head: String,
    },
    Ejected {
        head: String,
        reason: String,
        #[serde(default)]
        seq: u64,
    },
    Enqueued {
        head: String,
        #[serde(default)]
        seq: u64,
    },
    Verdict {
        verdict: String,
        head: String,
    },
    RequiredCheck {
        result: String,
        names: Vec<String>,
        head: String,
        #[serde(default)]
        seq: u64,
    },
    DirtyAfterBaseMove {
        head: String,
        base: String,
        base_oid: String,
    },
    ClaimTaken {
        by: String,
        until: String,
        head: String,
    },
    StackedBaseMerged {
        base_pr: Option<u64>,
        head: String,
    },
}

impl RelayEvent {
    /// The per-PR sequence number of a repeatable event (one that can
    /// recur at the same head: enqueued, ejected, required checks).
    pub(crate) fn seq_mut(&mut self) -> Option<&mut u64> {
        match self {
            Self::Ejected { seq, .. }
            | Self::Enqueued { seq, .. }
            | Self::RequiredCheck { seq, .. } => Some(seq),
            _ => None,
        }
    }

    /// The event's name, as `relay.events` lists it.
    pub(crate) fn name(&self) -> &'static str {
        match self {
            Self::Merged { .. } => "merged",
            Self::Closed { .. } => "closed",
            Self::Ejected { .. } => "ejected",
            Self::Enqueued { .. } => "enqueued",
            Self::Verdict { .. } => "verdict",
            Self::RequiredCheck { .. } => "required_check",
            Self::DirtyAfterBaseMove { .. } => "dirty_after_base_move",
            Self::ClaimTaken { .. } => "claim_taken",
            Self::StackedBaseMerged { .. } => "stacked_base_merged",
        }
    }

    /// Merged or closed: delivered without waiting out the rate limit.
    pub(crate) fn terminal(&self) -> bool {
        matches!(self, Self::Merged { .. } | Self::Closed { .. })
    }

    /// Idempotency key: the same fact about the same head is delivered once.
    pub(crate) fn key(&self, repo: &str, pr: u64) -> String {
        let detail = match self {
            Self::Merged { head } | Self::Closed { head } => head.clone(),
            Self::Enqueued { head, seq } | Self::Ejected { head, seq, .. } => {
                format!("{head}:{seq}")
            }
            Self::Verdict { verdict, head } => format!("{head}:{verdict}"),
            Self::RequiredCheck {
                result,
                names,
                head,
                seq,
            } => format!("{head}:{result}:{}:{seq}", names.join(",")),
            Self::DirtyAfterBaseMove { head, base_oid, .. } => format!("{head}:{base_oid}"),
            Self::ClaimTaken { by, until, .. } => format!("{by}:{until}"),
            Self::StackedBaseMerged { base_pr, head } => {
                format!(
                    "{head}:{}",
                    base_pr.map_or("?".to_owned(), |n| n.to_string())
                )
            }
        };
        format!("{repo}#{pr}:{}:{detail}", self.name())
    }

    /// What changed, in a few words.
    pub(crate) fn describe(&self) -> String {
        let short = |head: &str| head.get(..10).unwrap_or(head).to_owned();
        match self {
            Self::Merged { .. } => "merged".to_owned(),
            Self::Closed { .. } => "closed without merging".to_owned(),
            Self::Ejected { head, reason, .. } => {
                format!("ejected from the merge queue at {} ({reason})", short(head))
            }
            Self::Enqueued { head, .. } => format!("entered the merge queue at {}", short(head)),
            Self::Verdict { verdict, head } => {
                format!("review {verdict} at your head {}", short(head))
            }
            Self::RequiredCheck {
                result,
                names,
                head,
                ..
            } => {
                if result == "red" {
                    format!(
                        "required check(s) red at {}: {}",
                        short(head),
                        names.join(", ")
                    )
                } else {
                    format!("required checks green at {}", short(head))
                }
            }
            Self::DirtyAfterBaseMove { base, .. } => {
                format!("now conflicts with {base} after {base} moved (your head did not change)")
            }
            Self::ClaimTaken { by, until, .. } => format!("repair-claim by {by} until {until}"),
            Self::StackedBaseMerged { base_pr, .. } => match base_pr {
                Some(n) => format!("its stacked base #{n} merged"),
                None => "its stacked base merged (retargeted)".to_owned(),
            },
        }
    }
}

/// The events between the last digest and this one. A first sighting has
/// none: it is the baseline the next pass compares against.
pub(crate) fn derive_events(old: Option<&PrDigest>, new: &PrDigest) -> Vec<RelayEvent> {
    let Some(old) = old else {
        return Vec::new();
    };
    if old.terminal() || new.state == "MISSING" {
        return Vec::new();
    }
    let head = new.head.clone();
    if new.state == "MERGED" {
        return vec![RelayEvent::Merged { head }];
    }
    if new.state == "CLOSED" {
        return vec![RelayEvent::Closed { head }];
    }
    let mut events = Vec::new();
    if old.in_queue && !new.in_queue {
        let reason = if new.checks == Checks::Failure && !new.red.is_empty() {
            format!("red: {}", new.red.join(", "))
        } else if new.merge == "DIRTY" {
            "conflicts with the base".to_owned()
        } else {
            format!("merge state {}", new.merge)
        };
        events.push(RelayEvent::Ejected {
            head: head.clone(),
            reason,
            seq: 0,
        });
    } else if !old.in_queue && new.in_queue {
        events.push(RelayEvent::Enqueued {
            head: head.clone(),
            seq: 0,
        });
    }
    if let Some(verdict) = &new.verdict
        && (old.verdict.as_ref() != Some(verdict) || old.head != new.head)
    {
        events.push(RelayEvent::Verdict {
            verdict: verdict.clone(),
            head: head.clone(),
        });
    }
    let head_moved = old.head != new.head;
    match new.checks {
        Checks::Failure if old.checks != Checks::Failure || old.red != new.red || head_moved => {
            events.push(RelayEvent::RequiredCheck {
                result: "red".to_owned(),
                names: new.red.clone(),
                head: head.clone(),
                seq: 0,
            });
        }
        Checks::Success if old.checks != Checks::Success || head_moved => {
            events.push(RelayEvent::RequiredCheck {
                result: "green".to_owned(),
                names: Vec::new(),
                head: head.clone(),
                seq: 0,
            });
        }
        _ => {}
    }
    let base_moved = old.base_oid.is_empty() || old.base_oid != new.base_oid;
    if new.merge == "DIRTY" && old.merge != "DIRTY" && !head_moved && base_moved {
        events.push(RelayEvent::DirtyAfterBaseMove {
            head: head.clone(),
            base: new.base.clone(),
            base_oid: new.base_oid.clone(),
        });
    }
    if let Some(claim) = &new.claim
        && old.claim.as_ref() != Some(claim)
    {
        events.push(RelayEvent::ClaimTaken {
            by: claim.by.clone(),
            until: claim.until.clone(),
            head: head.clone(),
        });
    }
    let retargeted_to_default = old.base != old.default_branch
        && !old.default_branch.is_empty()
        && new.base == new.default_branch;
    if let Some(base_pr) = new.stacked_base_merged
        && old.stacked_base_merged != Some(base_pr)
    {
        events.push(RelayEvent::StackedBaseMerged {
            base_pr: Some(base_pr),
            head,
        });
    } else if retargeted_to_default && new.stacked_base_merged.is_none() {
        events.push(RelayEvent::StackedBaseMerged {
            base_pr: old.stacked_base_merged,
            head,
        });
    }
    events
}

fn verdict_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?mi)^\s*verdict:\s*(APPROVE|HOLD)\s+head:([0-9a-f]{40})").expect("regex")
    })
}

fn claim_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\s*repair-claim:\s*(\S+)\s+until\s+(\S+)").expect("regex"))
}

/// Whether `text` holds anything a merge or arm guard reads as a verdict: a
/// `verdict:` or `reviewed:` marker line, a session stamp, or the inline
/// spelling of either. The relay never sends such text.
pub(crate) fn refuses_verdict_text(text: &str) -> bool {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?i)verdict:\s*(APPROVE|HOLD)|reviewed:\s*[0-9a-f]{7,}|<!--\s*session:|post-merge:\s*OK")
            .expect("regex")
    });
    re.is_match(text)
}

/// A comment author whose verdict or claim the relay repeats: a repository
/// owner, member or collaborator, or a GitHub App (the fleet's reviewers and
/// lanes post through one). Outside contributors are not.
pub(crate) fn trusted_author(comment: &Value) -> bool {
    let association = comment
        .get("authorAssociation")
        .and_then(Value::as_str)
        .unwrap_or("");
    crate::head_approval::TRUSTED_ASSOCIATIONS.contains(&association)
        || comment
            .pointer("/author/__typename")
            .and_then(Value::as_str)
            == Some("Bot")
}

fn required_name(node: &Value) -> Option<String> {
    node.get("name")
        .or_else(|| node.get("context"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn check_result(node: &Value) -> &'static str {
    if node.get("name").is_none() {
        return match node.get("state").and_then(Value::as_str) {
            Some("SUCCESS") => "ok",
            Some("PENDING" | "EXPECTED") | None => "pending",
            Some(_) => "red",
        };
    }
    if node.get("status").and_then(Value::as_str) != Some("COMPLETED") {
        return "pending";
    }
    match node.get("conclusion").and_then(Value::as_str) {
        Some("SUCCESS" | "SKIPPED" | "NEUTRAL") => "ok",
        _ => "red",
    }
}

/// Verdict at `head`, newest trusted repair claim, and newest comment time
/// not written by the relay.
fn scan_comments(pr: &Value, head: &str) -> (Option<String>, Option<Claim>, Option<String>) {
    let comments = pr
        .pointer("/comments/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut verdict = None;
    let mut claim = None;
    let mut last_comment_at = None;
    for comment in &comments {
        let body = comment.get("body").and_then(Value::as_str).unwrap_or("");
        if !body.trim_start().starts_with(MESSAGE_PREFIX)
            && let Some(at) = comment.get("createdAt").and_then(Value::as_str)
        {
            last_comment_at = Some(at.to_owned());
        }
        // Anyone can comment. Only a trusted author's verdict or claim is
        // relayed, so a forged "verdict: APPROVE" cannot stand the owner down
        // and an outsider's text never reaches an agent's prompt.
        if !trusted_author(comment) {
            continue;
        }
        for found in verdict_regex().captures_iter(body) {
            if &found[2] == head {
                verdict = Some(found[1].to_ascii_uppercase());
            }
        }
        // Comment text reaches an agent's prompt: keep only plain tokens.
        if let Some(found) = claim_regex().captures_iter(body).last()
            && crate::owner_registry::safe_value(&found[1])
            && crate::owner_registry::safe_value(&found[2])
            && found[1].len() <= 64
            && found[2].len() <= 64
        {
            claim = Some(Claim {
                by: found[1].to_owned(),
                until: found[2].to_owned(),
            });
        }
    }
    (verdict, claim, last_comment_at)
}

/// Fold one `pullRequest` GraphQL node into a digest. `None` for a missing
/// node (deleted, or a number that never existed).
pub(crate) fn summarize(
    node: Option<&Value>,
    default_branch: &str,
    review_gates: &BTreeSet<String>,
) -> PrDigest {
    let Some(pr) = node.filter(|v| !v.is_null()) else {
        return PrDigest {
            state: "MISSING".to_owned(),
            merge: String::new(),
            head: String::new(),
            base: String::new(),
            base_oid: String::new(),
            default_branch: default_branch.to_owned(),
            in_queue: false,
            checks: Checks::Pending,
            red: Vec::new(),
            verdict: None,
            claim: None,
            stacked_base_merged: None,
            last_comment_at: None,
            node_id: String::new(),
        };
    };
    let text = |key: &str| pr.get(key).and_then(Value::as_str).unwrap_or("").to_owned();
    let head = text("headRefOid");
    let (verdict, claim, last_comment_at) = scan_comments(pr, &head);
    let contexts = pr
        .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/nodes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let required: Vec<&Value> = contexts
        .iter()
        .filter(|node| node.get("isRequired").and_then(Value::as_bool) == Some(true))
        .filter(|node| required_name(node).is_some_and(|name| !review_gates.contains(&name)))
        .collect();
    let mut red: Vec<String> = required
        .iter()
        .filter(|node| check_result(node) == "red")
        .filter_map(|node| required_name(node))
        .collect();
    red.sort();
    red.dedup();
    // More contexts than one page holds: an unread one may be a required
    // check still running, so never call it green.
    let truncated = pr
        .pointer("/commits/nodes/0/commit/statusCheckRollup/contexts/totalCount")
        .and_then(Value::as_u64)
        .is_some_and(|total| total > contexts.len() as u64);
    let checks = if !red.is_empty() {
        Checks::Failure
    } else if truncated
        || required.is_empty()
        || required.iter().any(|node| check_result(node) == "pending")
    {
        Checks::Pending
    } else {
        Checks::Success
    };
    let state = if pr.get("merged").and_then(Value::as_bool) == Some(true) {
        "MERGED".to_owned()
    } else {
        text("state")
    };
    // `associatedPullRequests` on the base ref lists pull requests whose
    // HEAD is that ref; it only means "stacked base" when the base is not the
    // default branch (a main-to-release sync PR has head main).
    let stacked_base_merged = (!default_branch.is_empty() && text("baseRefName") != default_branch)
        .then(|| {
            pr.pointer("/baseRef/associatedPullRequests/nodes/0/number")
                .and_then(Value::as_u64)
        })
        .flatten();
    PrDigest {
        state,
        merge: text("mergeStateStatus"),
        head,
        base: text("baseRefName"),
        base_oid: text("baseRefOid"),
        default_branch: default_branch.to_owned(),
        in_queue: pr.get("isInMergeQueue").and_then(Value::as_bool) == Some(true),
        checks,
        red,
        verdict,
        claim,
        stacked_base_merged,
        last_comment_at,
        node_id: text("id"),
    }
}

/// The next step for the owner, in the register `pr_relay.py` established.
pub(crate) fn next_step(digest: &PrDigest, pr: u64) -> String {
    if digest.state == "MERGED" {
        return "merged; drop it from your waits and move to your next item".to_owned();
    }
    if digest.state == "CLOSED" {
        return "closed without merging; check whether it was superseded".to_owned();
    }
    if digest.merge == "DIRTY" {
        return format!(
            "conflicts with {base}: merge origin/{base} (never rebase), regenerate generated files \
             with their generator, run gates, push once, comment 'repaired: ... at <sha>'. Check \
             for a fresh 'repair-claim:' first",
            base = digest.base
        );
    }
    if digest.verdict.as_deref() == Some("HOLD") {
        return "HOLD at your head: fix what it names with a test, push once, comment \
                'repaired: ... at <sha>'"
            .to_owned();
    }
    if digest.checks == Checks::Failure {
        return format!(
            "required check(s) red at your head ({}): run shipyard diagnose --pr {pr}; fix with a \
             test if this PR caused it, else re-run the failed job once",
            if digest.red.is_empty() {
                "?".to_owned()
            } else {
                digest.red.join(",")
            }
        );
    }
    if digest.verdict.as_deref() == Some("APPROVE") && digest.checks == Checks::Success {
        return "approved and green: the steward will enqueue it; nothing to do".to_owned();
    }
    if digest.verdict.is_none() && digest.checks == Checks::Success {
        return "green, waiting for the standing reviewer; nothing to do unless it stays \
                unreviewed past 30 min"
            .to_owned();
    }
    "checks running; this relay will message you when it changes, so work on something else \
     meanwhile (do not poll)"
        .to_owned()
}

/// The one line delivered for a batch of `events`. `steward_next` is the
/// steward tick's `next_action` for this head, when fresh.
pub(crate) fn render_message(
    repo: &str,
    pr: u64,
    digest: &PrDigest,
    events: &[RelayEvent],
    steward_next: Option<&str>,
) -> String {
    let what = events
        .iter()
        .map(RelayEvent::describe)
        .collect::<Vec<_>>()
        .join("; ");
    let next = steward_next.map_or_else(
        || next_step(digest, pr),
        |text| format!("(steward) {}", text.trim()),
    );
    let verdict = digest.verdict.as_deref().unwrap_or("none");
    let text = format!(
        "{MESSAGE_PREFIX} {repo}#{pr} {what}. Now: state={} merge={} checks={:?} review={verdict} \
         head={}. Next: {next}",
        digest.state,
        digest.merge,
        digest.checks,
        digest.short_head(),
    );
    // One line: a newline could start a line a guard reads.
    text.replace(['\r', '\n'], " ")
}

/// The GraphQL fields read for one owned pull request, as alias `pr<N>`.
pub(crate) fn pr_fields(number: u64) -> String {
    format!(
        "pr{number}: pullRequest(number:{number}){{id number state merged isDraft isInMergeQueue \
         mergeStateStatus headRefOid baseRefName baseRefOid \
         baseRef{{associatedPullRequests(states:[MERGED],last:1){{nodes{{number}}}}}} \
         comments(last:30){{nodes{{body createdAt authorAssociation author{{__typename login}}}}}} \
         commits(last:1){{nodes{{commit{{statusCheckRollup{{contexts(first:100){{totalCount nodes{{\
         ... on CheckRun{{name status conclusion isRequired(pullRequestNumber:{number})}} \
         ... on StatusContext{{context state isRequired(pullRequestNumber:{number})}}}}}}}}}}}}}}}}"
    )
}

/// Owned pull requests per batched query: bounds the query cost.
pub(crate) const MAX_PRS_PER_QUERY: usize = 40;

/// One repository's query: the default branch, every number in `numbers`
/// and, with `discover`, the bodies of the open pull requests (newest
/// first), whose owner markers name owners registered on other hosts.
pub(crate) fn repo_query(numbers: &[u64], discover: bool) -> String {
    let fields = numbers
        .iter()
        .map(|n| pr_fields(*n))
        .collect::<Vec<_>>()
        .join(" ");
    let open = if discover {
        "open: pullRequests(states:OPEN,first:100,orderBy:{field:UPDATED_AT,direction:DESC}){nodes{number body}}"
    } else {
        ""
    };
    format!(
        "query($owner:String!,$name:String!){{repository(owner:$owner,name:$name){{\
         defaultBranchRef{{name}} {open} {fields}}}}}"
    )
}

/// What one repository's read returned.
#[derive(Debug, Default)]
pub(crate) struct RepoRead {
    pub(crate) default_branch: String,
    pub(crate) prs: BTreeMap<u64, Option<Value>>,
    /// Open pull-request bodies that carry an owner marker, by number.
    pub(crate) markers: BTreeMap<u64, String>,
}

/// Read `numbers` (and, with `discover`, the marked open pull requests) of
/// `repo` with as few GraphQL queries as [`MAX_PRS_PER_QUERY`] allows.
pub(crate) fn fetch(
    gh: crate::owner_registry::Gh<'_>,
    repo: &str,
    numbers: &[u64],
    discover: bool,
) -> Result<RepoRead, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository {repo:?}"))?;
    let mut read = RepoRead::default();
    let mut chunks: Vec<&[u64]> = numbers.chunks(MAX_PRS_PER_QUERY).collect();
    if chunks.is_empty() {
        chunks.push(&[]);
    }
    for (index, chunk) in chunks.iter().enumerate() {
        let with_discovery = discover && index == 0;
        let args = vec![
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={}", repo_query(chunk, with_discovery)),
            "-f".to_owned(),
            format!("owner={owner}"),
            "-f".to_owned(),
            format!("name={name}"),
        ];
        let raw = gh(&args)?;
        let value: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("unparseable GraphQL answer: {error}"))?;
        if let Some(errors) = value.get("errors").and_then(Value::as_array)
            && value.pointer("/data/repository").is_none_or(Value::is_null)
        {
            return Err(format!("GraphQL errors: {}", Value::Array(errors.clone())));
        }
        let repository = value
            .pointer("/data/repository")
            .ok_or_else(|| "GraphQL answer has no repository".to_owned())?;
        if let Some(branch) = repository
            .pointer("/defaultBranchRef/name")
            .and_then(Value::as_str)
        {
            branch.clone_into(&mut read.default_branch);
        }
        for number in *chunk {
            let node = repository
                .get(format!("pr{number}"))
                .filter(|v| !v.is_null())
                .cloned();
            read.prs.insert(*number, node);
        }
        if with_discovery {
            for node in repository
                .pointer("/open/nodes")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let (Some(number), Some(body)) = (
                    node.get("number").and_then(Value::as_u64),
                    node.get("body").and_then(Value::as_str),
                ) && body.contains(crate::owner_registry::MARKER_PREFIX)
                {
                    read.markers.insert(number, body.to_owned());
                }
            }
        }
    }
    Ok(read)
}

/// The steward's per-pass report, reduced to what the relay reuses: for each
/// pull request, the head it judged and its `next_action`.
pub(crate) fn steward_next_actions(report: &Value) -> BTreeMap<u64, (String, String)> {
    let mut out = BTreeMap::new();
    for repo in report
        .get("repos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        for pr in repo
            .get("prs")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let (Some(number), Some(head), Some(action)) = (
                pr.get("number").and_then(Value::as_u64),
                pr.get("head_sha").and_then(Value::as_str),
                pr.pointer("/next_action/action").and_then(Value::as_str),
            ) {
                out.insert(number, (head.to_owned(), action.to_owned()));
            }
        }
    }
    out
}

/// One delivery, as appended to the delivery log.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct DeliveryRecord {
    /// Always `delivery`.
    pub(crate) kind: String,
    pub(crate) at: DateTime<Utc>,
    pub(crate) mode: String,
    pub(crate) repo: String,
    pub(crate) pr: u64,
    pub(crate) head: String,
    pub(crate) events: Vec<RelayEvent>,
    pub(crate) keys: Vec<String>,
    /// `owner`, `lead`, `repair_lane`, `comment`, or `none`.
    pub(crate) tier: String,
    pub(crate) transport: Option<String>,
    pub(crate) delivered: bool,
    #[serde(default)]
    pub(crate) attempts: Vec<String>,
    #[serde(default)]
    pub(crate) refused: Option<String>,
    pub(crate) message: String,
}

/// The owner's first action after a delivery, appended to the delivery log.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ActedRecord {
    /// Always `acted`.
    pub(crate) kind: String,
    pub(crate) repo: String,
    pub(crate) pr: u64,
    pub(crate) delivered_at: DateTime<Utc>,
    /// When the relay observed the action (the pass after it happened).
    pub(crate) acted_at: Option<DateTime<Utc>>,
    /// `push`, `comment`, `merged`, `closed`, or `none` (gave up).
    pub(crate) action: String,
    /// The pull request's state when the action was seen.
    pub(crate) outcome: String,
    pub(crate) keys: Vec<String>,
}
