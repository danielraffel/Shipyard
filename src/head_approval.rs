//! Arm native auto-merge only on a head a reviewer approved.
//!
//! ## Why arming belongs to approval
//!
//! Arming is the decision to land. Shipyard arms from three places — when a
//! pull request is opened or re-shipped, after its own validation, and in the
//! merge steward's backstop — and each of them used to arm whatever head was
//! current. A pull request whose branch is rebased and force-pushed therefore
//! came back armed seconds after every push, so the queue spent a full gate on
//! each head before anyone had looked at it, and a pull request opened unarmed
//! was armed by the next re-ship. Approval is a fact about one exact head, so
//! the check lives at the arming boundary rather than in any one caller.
//!
//! ## The setting
//!
//! `[auto_merge] arm_requires_head_approval = true` in the repository's
//! `.shipyard/config.toml` turns the check on. It is off by default, so a
//! repository that wants arming as a durability backstop keeps it. The setting
//! and the reviewer allowlist are read from the protected base branch through
//! the GitHub API, never from the head being armed: a branch must not be able
//! to switch off its own gate or list itself as a reviewer.
//!
//! ## What counts as approval
//!
//! * A pull-request review in state `APPROVED` whose `commit_id` is the head,
//!   from an account that is not a bot.
//! * A comment carrying a line `reviewed:<sha>` (seven or more hex digits, a
//!   prefix of the head), from a login in `[auto_merge] reviewer_logins` that
//!   is not a bot. The list is empty by default, so markers count only once a
//!   repository names its reviewers.
//!
//! Bot accounts never count, including the one Shipyard itself posts through:
//! every agent shares that identity, so a marker it wrote would approve itself.
//!
//! ## Ejections
//!
//! The merge queue records why it removed each entry. After
//! [`EJECTION_CAP`] removals of the same head, the head is not armed again
//! until it carries an approval newer than the last removal. A new head starts
//! its own count.

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

/// Config table holding the settings this module reads.
pub const CONFIG_TABLE: &str = "auto_merge";
/// Key turning the check on.
pub const REQUIRED_KEY: &str = "arm_requires_head_approval";
/// Key listing logins whose `reviewed:<sha>` comments count as approval.
pub const REVIEWERS_KEY: &str = "reviewer_logins";
/// Tracked config file the setting is read from on the protected base.
pub const CONFIG_PATH: &str = ".shipyard/config.toml";
/// Removals of one head after which it needs a fresh approval to re-arm.
pub const EJECTION_CAP: usize = 2;

/// A `gh` transport. Returns stdout, or a message that includes stderr.
pub type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// The repository's approval policy.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ApprovalPolicy {
    /// Whether arming requires an approval of the exact head.
    pub required: bool,
    /// Logins whose `reviewed:<sha>` comments count, lower-cased.
    pub reviewer_logins: Vec<String>,
}

impl ApprovalPolicy {
    /// Read the policy from a parsed `.shipyard/config.toml`.
    ///
    /// An absent table or key is the default: not required, no reviewers.
    #[must_use]
    pub fn from_table(table: &toml::Table) -> Self {
        let Some(section) = table.get(CONFIG_TABLE).and_then(toml::Value::as_table) else {
            return Self::default();
        };
        Self {
            required: section
                .get(REQUIRED_KEY)
                .and_then(toml::Value::as_bool)
                .unwrap_or(false),
            reviewer_logins: section
                .get(REVIEWERS_KEY)
                .and_then(toml::Value::as_array)
                .map(|logins| {
                    logins
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_ascii_lowercase)
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// How an approval was given.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "via", rename_all = "snake_case")]
pub enum Approval {
    /// A review in state `APPROVED` on the head.
    Review {
        /// Reviewer login.
        login: String,
    },
    /// A `reviewed:<sha>` comment from an allowlisted reviewer.
    Marker {
        /// Commenter login.
        login: String,
    },
}

impl Approval {
    /// One phrase naming who approved and how.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Review { login } => format!("an approving review by {login}"),
            Self::Marker { login } => format!("a reviewed:<sha> marker by {login}"),
        }
    }
}

/// One merge-queue removal of the head being armed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Ejection {
    /// GitHub's removal reason, as GitHub spells it.
    pub reason: String,
    /// What the reason means for the head, in plain words.
    pub cause: &'static str,
    /// When the removal happened.
    pub at: Option<String>,
}

/// What a removal reason says about the head.
///
/// GitHub does not say whether `failed_checks` was this head's own failure or
/// a batch-mate's under `ALLGREEN` grouping, so that reason is named as either.
#[must_use]
pub fn ejection_cause(reason: &str) -> &'static str {
    match reason.to_ascii_lowercase().as_str() {
        "manual" => "removed by hand",
        "failed_checks" => "checks failed, its own or a batch neighbour's",
        "merge_conflict" | "invalid_merge_commit" => "main moved under it",
        "branch_protection" | "branch_protections" => "branch protection changed",
        "rollback" => "the queue rolled back",
        _ => "another reason",
    }
}

/// Every non-merge queue removal of `head` in a [`crate::pr_queue_state`]
/// timeline response, oldest first.
#[must_use]
pub fn head_ejections(queue: &Value, head: &str) -> Vec<Ejection> {
    let nodes = queue
        .pointer("/data/repository/pullRequest/timelineItems/nodes")
        .and_then(Value::as_array);
    nodes
        .into_iter()
        .flatten()
        .filter(|node| {
            node.get("__typename").and_then(Value::as_str) == Some("RemovedFromMergeQueueEvent")
        })
        .filter(|node| {
            node.pointer("/beforeCommit/oid")
                .and_then(Value::as_str)
                .is_some_and(|oid| oid.eq_ignore_ascii_case(head))
        })
        .filter_map(|node| {
            let reason = node.get("reason").and_then(Value::as_str)?;
            (!reason.eq_ignore_ascii_case("merged")).then(|| Ejection {
                reason: reason.to_owned(),
                cause: ejection_cause(reason),
                at: node
                    .get("createdAt")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })
        })
        .collect()
}

fn is_bot(record: &Value) -> bool {
    record
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bot"))
        || login(record).is_some_and(|login| login.ends_with("[bot]"))
}

fn login(record: &Value) -> Option<&str> {
    record.get("login").and_then(Value::as_str)
}

fn at(record: &Value) -> Option<DateTime<Utc>> {
    record
        .get("at")
        .and_then(Value::as_str)
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

fn marker_names_head(body: &str, head: &str) -> bool {
    let pattern =
        Regex::new(r"(?im)^\s*reviewed:([0-9a-f]{7,40})\s*$").expect("marker pattern compiles");
    let head = head.to_ascii_lowercase();
    pattern
        .captures_iter(body)
        .any(|capture| head.starts_with(&capture[1].to_ascii_lowercase()))
}

/// The newest approval of `head`, given reviews and comments flattened to
/// `{login, type, at, state, commit_id}` / `{login, type, at, body}` records.
///
/// `newer_than` drops approvals given at or before that instant; an approval
/// whose time is unreadable cannot prove it is newer and is dropped too.
#[must_use]
pub fn find_approval(
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    policy: &ApprovalPolicy,
    newer_than: Option<DateTime<Utc>>,
) -> Option<Approval> {
    let fresh =
        |record: &Value| newer_than.is_none_or(|floor| at(record).is_some_and(|at| at > floor));
    let review = reviews.iter().rev().find(|review| {
        !is_bot(review)
            && fresh(review)
            && review
                .get("state")
                .and_then(Value::as_str)
                .is_some_and(|state| state.eq_ignore_ascii_case("APPROVED"))
            && review
                .get("commit_id")
                .and_then(Value::as_str)
                .is_some_and(|commit| commit.eq_ignore_ascii_case(head))
    });
    if let Some(login) = review.and_then(login) {
        return Some(Approval::Review {
            login: login.to_owned(),
        });
    }
    comments
        .iter()
        .rev()
        .find(|comment| {
            !is_bot(comment)
                && fresh(comment)
                && login(comment).is_some_and(|login| {
                    policy
                        .reviewer_logins
                        .iter()
                        .any(|reviewer| reviewer.eq_ignore_ascii_case(login))
                })
                && comment
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| marker_names_head(body, head))
        })
        .and_then(login)
        .map(|login| Approval::Marker {
            login: login.to_owned(),
        })
}

/// Whether the head may be armed.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "gate", rename_all = "snake_case")]
pub enum HeadGate {
    /// The repository does not require approval.
    NotRequired,
    /// Approved; arm it.
    Approved {
        /// Who approved, and how.
        approval: Approval,
    },
    /// No approval names this head.
    NotApproved {
        /// The head that needs one.
        head: String,
    },
    /// The queue removed this head [`EJECTION_CAP`] or more times and no
    /// approval followed the last removal.
    EjectionCap {
        /// The head.
        head: String,
        /// Every removal of it, oldest first.
        ejections: Vec<Ejection>,
    },
}

impl HeadGate {
    /// Whether arming may proceed.
    #[must_use]
    pub const fn allows(&self) -> bool {
        matches!(self, Self::NotRequired | Self::Approved { .. })
    }

    /// One line for the ship transcript or the steward report.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::NotRequired => "this repository does not require head approval".to_owned(),
            Self::Approved { approval } => format!("the head carries {}", approval.describe()),
            Self::NotApproved { head } => format!(
                "head {} carries no approval (an APPROVED review on it, or a reviewed:{} \
                 marker from a listed reviewer), and this repository arms only approved heads; \
                 left disarmed. Arm deliberately with `--arm`",
                short(head),
                short(head)
            ),
            Self::EjectionCap { head, ejections } => format!(
                "the merge queue removed head {} {} times ({}), and no approval followed the \
                 last removal; left disarmed until a reviewer approves it again, a new head is \
                 pushed, or someone arms it with `--arm`",
                short(head),
                ejections.len(),
                ejections
                    .iter()
                    .map(|ejection| format!("{}: {}", ejection.reason, ejection.cause))
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
        }
    }
}

fn short(head: &str) -> &str {
    head.get(..12).unwrap_or(head)
}

/// Decide from already-read facts.
#[must_use]
pub fn decide(
    policy: &ApprovalPolicy,
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    queue: &Value,
) -> HeadGate {
    if !policy.required {
        return HeadGate::NotRequired;
    }
    let ejections = head_ejections(queue, head);
    if ejections.len() >= EJECTION_CAP {
        // An unreadable removal time leaves no floor an approval can be
        // proven newer than, so nothing passes it.
        let floor = ejections
            .last()
            .and_then(|ejection| ejection.at.as_deref())
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map_or(DateTime::<Utc>::MAX_UTC, |at| at.with_timezone(&Utc));
        return match find_approval(head, reviews, comments, policy, Some(floor)) {
            Some(approval) => HeadGate::Approved { approval },
            None => HeadGate::EjectionCap {
                head: head.to_owned(),
                ejections,
            },
        };
    }
    match find_approval(head, reviews, comments, policy, None) {
        Some(approval) => HeadGate::Approved { approval },
        None => HeadGate::NotApproved {
            head: head.to_owned(),
        },
    }
}

/// Read the policy from `base` on GitHub. A missing file is the default.
///
/// # Errors
///
/// When the file exists but cannot be read or parsed: an unreadable policy is
/// not an absent one.
pub fn read_policy(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<ApprovalPolicy, String> {
    let raw = match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/{CONFIG_PATH}?ref={base}"),
    ]) {
        Ok(raw) => raw,
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => {
            return Ok(ApprovalPolicy::default());
        }
        Err(detail) => return Err(format!("{CONFIG_PATH} on {base} unreadable: {detail}")),
    };
    raw.parse::<toml::Table>()
        .map(|table| ApprovalPolicy::from_table(&table))
        .map_err(|error| format!("{CONFIG_PATH} on {base} does not parse: {error}"))
}

fn read_records(run_gh: RunGh<'_>, path: &str, jq: &str) -> Result<Vec<Value>, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        "--paginate".to_owned(),
        path.to_owned(),
        "--jq".to_owned(),
        jq.to_owned(),
    ])?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| format!("malformed {path} record: {error}"))
        })
        .collect()
}

/// Decide for one pull request, reading what the decision needs.
///
/// A repository that does not require approval costs one read.
///
/// # Errors
///
/// When a needed fact cannot be read. Callers must not arm on an error.
pub fn evaluate(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    base: &str,
    queue: &Value,
) -> Result<HeadGate, String> {
    let policy = read_policy(run_gh, repo, base)?;
    evaluate_with_policy(run_gh, repo, pr, head, &policy, queue)
}

/// [`evaluate`] with the policy already read, for a caller that reads the
/// queue state only when the policy needs it.
///
/// # Errors
///
/// When a needed fact cannot be read. Callers must not arm on an error.
pub fn evaluate_with_policy(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    policy: &ApprovalPolicy,
    queue: &Value,
) -> Result<HeadGate, String> {
    if !policy.required {
        return Ok(HeadGate::NotRequired);
    }
    let reviews = read_records(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews"),
        ".[] | {login: .user.login, type: .user.type, at: .submitted_at, state: .state, commit_id: .commit_id}",
    )?;
    let comments = read_records(
        run_gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        ".[] | {login: .user.login, type: .user.type, at: .created_at, body: .body}",
    )?;
    Ok(decide(policy, head, &reviews, &comments, queue))
}

#[cfg(test)]
pub(crate) mod tests;
