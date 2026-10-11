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
//!   from a configured reviewer identity.
//! * A final-line `verdict: APPROVE head:<sha>` or `reviewed:<sha>` comment
//!   from a configured reviewer identity. The sha is always the full head.
//!   `verdict: HOLD head:<sha>` and a matching `REQUEST_CHANGES` review block.
//!
//! Bot status alone is not a rejection: the dedicated review App is configured
//! as a reviewer. The fleet author App and the PR author cannot approve their
//! own head.
//!
//! ## Ejections
//!
//! The merge queue records why it removed each entry. After
//! [`EJECTION_CAP`] removals of the same head, the head is not armed again
//! until it carries an approval newer than the last removal. A new head starts
//! its own count. Removals are read from the merge-queue query's
//! `timelineItems(last:100)` window, so a head with more than 100 later events
//! can be under-counted; that errs lenient, because an approval is still
//! required.

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
/// New spelling for the configured reviewer identities.
pub const REVIEWER_IDENTITIES_KEY: &str = "reviewer_identities";
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
                .get(REVIEWER_IDENTITIES_KEY)
                .or_else(|| section.get(REVIEWERS_KEY))
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

    /// Reject a policy that would let Shipyard approve its own PR.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .reviewer_logins
            .iter()
            .any(|login| login.eq_ignore_ascii_case("shipyard-local[bot]"))
        {
            return Err("reviewer_identities must not include shipyard-local[bot]".to_owned());
        }
        Ok(())
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
        /// GitHub App database id that performed this review, when present.
        #[serde(default)]
        performed_via_github_app_id: Option<u64>,
    },
    /// A `reviewed:<sha>` comment from an allowlisted reviewer.
    Marker {
        /// Commenter login.
        login: String,
        /// GitHub comment id used as durable approval evidence.
        #[serde(default)]
        comment_id: Option<u64>,
    },
}

impl Approval {
    /// One phrase naming who approved and how.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Review { login, .. } => format!("an approving review by {login}"),
            Self::Marker { login, .. } => format!("a reviewed:<sha> marker by {login}"),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Verdict {
    Approve,
    Hold,
}

fn marker_verdict(body: &str) -> Option<(Verdict, String)> {
    let pattern = Regex::new(
        r"^[ \t]*(?:reviewed:[ \t]*([0-9a-f]{40})|verdict:[ \t]*(APPROVE|HOLD)[ \t]+head:[ \t]*([0-9a-f]{40}))[ \t]*$",
    )
    .expect("marker pattern compiles");
    let line = body.lines().rev().find(|line| !line.trim().is_empty())?;
    pattern.captures(line).and_then(|capture| {
        if let Some(head) = capture.get(1) {
            return Some((Verdict::Approve, head.as_str().to_ascii_lowercase()));
        }
        let verdict = match capture.get(2)?.as_str().to_ascii_uppercase().as_str() {
            "APPROVE" => Verdict::Approve,
            "HOLD" => Verdict::Hold,
            _ => return None,
        };
        Some((verdict, capture.get(3)?.as_str().to_ascii_lowercase()))
    })
}

fn marker_names_head(body: &str, head: &str) -> bool {
    marker_verdict(body).is_some_and(|(verdict, marker_head)| {
        verdict == Verdict::Approve && marker_head.eq_ignore_ascii_case(head)
    })
}

fn normalized_login(value: &str) -> String {
    let value = value.strip_prefix("app/").unwrap_or(value);
    value
        .strip_suffix("[bot]")
        .unwrap_or(value)
        .to_ascii_lowercase()
}

fn is_fleet_author(login: &str) -> bool {
    normalized_login(login) == "shipyard-local"
}

fn reviewer_allowed(login: &str, policy: &ApprovalPolicy, author: Option<&str>) -> bool {
    !is_fleet_author(login)
        && policy
            .reviewer_logins
            .iter()
            .any(|reviewer| reviewer.eq_ignore_ascii_case(login))
        && author.is_none_or(|author| !author.eq_ignore_ascii_case(login))
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
    find_approval_for_author(head, reviews, comments, policy, newer_than, None)
}

fn find_approval_for_author(
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    policy: &ApprovalPolicy,
    newer_than: Option<DateTime<Utc>>,
    author: Option<&str>,
) -> Option<Approval> {
    let fresh =
        |record: &Value| newer_than.is_none_or(|floor| at(record).is_some_and(|at| at > floor));
    let review = reviews.iter().rev().find(|review| {
        fresh(review)
            && login(review).is_some_and(|login| reviewer_allowed(login, policy, author))
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
            performed_via_github_app_id: review
                .and_then(|review| review.get("performed_via_github_app_id"))
                .and_then(Value::as_u64),
        });
    }
    comments
        .iter()
        .rev()
        .find(|comment| {
            fresh(comment)
                && login(comment).is_some_and(|login| reviewer_allowed(login, policy, author))
                && comment
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| marker_names_head(body, head))
        })
        .and_then(|comment| {
            Some(Approval::Marker {
                login: login(comment)?.to_owned(),
                comment_id: comment.get("id").and_then(Value::as_u64),
            })
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
    decide_excusing(policy, head, reviews, comments, queue, false)
}

/// [`decide`], leaving the latest removal out of [`EJECTION_CAP`] when
/// `latest_was_interruption`: a removal the classifier proved was an
/// interruption ([`crate::environment_requeue::RequeueClass::Interruption`])
/// says nothing against the head, so it must not cost the head its approval.
/// The interruption allowance is bounded on its own
/// ([`crate::environment_requeue::INTERRUPTION_REARM_LIMIT`]).
#[must_use]
pub fn decide_excusing(
    policy: &ApprovalPolicy,
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    queue: &Value,
    latest_was_interruption: bool,
) -> HeadGate {
    decide_for_author(
        policy,
        head,
        reviews,
        comments,
        queue,
        latest_was_interruption,
        None,
    )
}

fn latest_verdict(
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> Option<Verdict> {
    let review_events = reviews.iter().filter_map(|review| {
        let login = login(review)?;
        if !reviewer_allowed(login, policy, author)
            || !review
                .get("commit_id")
                .and_then(Value::as_str)
                .is_some_and(|commit| commit.eq_ignore_ascii_case(head))
        {
            return None;
        }
        let state = review.get("state")?.as_str()?;
        let verdict = if state.eq_ignore_ascii_case("APPROVED") {
            Verdict::Approve
        } else if state.eq_ignore_ascii_case("CHANGES_REQUESTED") {
            Verdict::Hold
        } else {
            return None;
        };
        Some((at(review)?, verdict))
    });
    let comment_events = comments.iter().filter_map(|comment| {
        let login = login(comment)?;
        if !reviewer_allowed(login, policy, author) {
            return None;
        }
        let (verdict, marker_head) = marker_verdict(comment.get("body")?.as_str()?)?;
        if marker_head != head.to_ascii_lowercase() {
            return None;
        }
        Some((at(comment)?, verdict))
    });
    review_events
        .chain(comment_events)
        .max_by_key(|(timestamp, _)| *timestamp)
        .map(|(_, verdict)| verdict)
}

pub(crate) fn decide_for_author(
    policy: &ApprovalPolicy,
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    queue: &Value,
    latest_was_interruption: bool,
    author: Option<&str>,
) -> HeadGate {
    if !policy.required {
        return HeadGate::NotRequired;
    }
    if latest_verdict(head, reviews, comments, policy, author) == Some(Verdict::Hold) {
        return HeadGate::NotApproved {
            head: head.to_owned(),
        };
    }
    let mut ejections = head_ejections(queue, head);
    if latest_was_interruption {
        ejections.pop();
    }
    if ejections.len() >= EJECTION_CAP {
        // An unreadable removal time leaves no floor an approval can be
        // proven newer than, so nothing passes it.
        let floor = ejections
            .last()
            .and_then(|ejection| ejection.at.as_deref())
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map_or(DateTime::<Utc>::MAX_UTC, |at| at.with_timezone(&Utc));
        return match find_approval_for_author(head, reviews, comments, policy, Some(floor), author)
        {
            Some(approval) => HeadGate::Approved { approval },
            None => HeadGate::EjectionCap {
                head: head.to_owned(),
                ejections,
            },
        };
    }
    match find_approval_for_author(head, reviews, comments, policy, None, author) {
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
fn read_policy_inner(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    missing_is_error: bool,
) -> Result<ApprovalPolicy, String> {
    let raw = match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/{CONFIG_PATH}?ref={base}"),
    ]) {
        Ok(raw) => raw,
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => {
            if missing_is_error {
                return Err(format!(
                    "{CONFIG_PATH} on {base} is absent; approval policy is required"
                ));
            }
            return Ok(ApprovalPolicy::default());
        }
        Err(detail) => return Err(format!("{CONFIG_PATH} on {base} unreadable: {detail}")),
    };
    let table = raw
        .parse::<toml::Table>()
        .map_err(|error| format!("{CONFIG_PATH} on {base} does not parse: {error}"))?;
    let policy = ApprovalPolicy::from_table(&table);
    policy.validate().map(|()| policy)
}

/// Read the policy from `base` on GitHub, treating a missing file as default.
pub fn read_policy(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<ApprovalPolicy, String> {
    read_policy_inner(run_gh, repo, base, false)
}

/// Read the policy for the steward enqueue gate, where absence is unsafe.
pub fn read_policy_required(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
) -> Result<ApprovalPolicy, String> {
    read_policy_inner(run_gh, repo, base, true)
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
    evaluate_excusing(run_gh, repo, pr, head, base, queue, false)
}

/// [`evaluate`] through [`decide_excusing`].
///
/// # Errors
///
/// When a needed fact cannot be read. Callers must not arm on an error.
pub fn evaluate_excusing(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    base: &str,
    queue: &Value,
    latest_was_interruption: bool,
) -> Result<HeadGate, String> {
    let policy = read_policy(run_gh, repo, base)?;
    if !policy.required {
        return Ok(HeadGate::NotRequired);
    }
    let reviews = read_records(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews"),
        ".[] | {login: .user.login, type: .user.type, at: .submitted_at, state: .state, commit_id: .commit_id, performed_via_github_app_id: .performed_via_github_app.id}",
    )?;
    let comments = read_records(
        run_gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        ".[] | {id: .id, login: .user.login, type: .user.type, at: .created_at, body: .body}",
    )?;
    Ok(decide_excusing(
        &policy,
        head,
        &reviews,
        &comments,
        queue,
        latest_was_interruption,
    ))
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
    evaluate_with_policy_for_author(run_gh, repo, pr, head, policy, queue, None)
}

/// Evaluate with the observed PR author available for self-review rejection.
pub fn evaluate_with_policy_for_author(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    policy: &ApprovalPolicy,
    queue: &Value,
    author: Option<&str>,
) -> Result<HeadGate, String> {
    if !policy.required {
        return Ok(HeadGate::NotRequired);
    }
    let reviews = read_records(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews"),
        ".[] | {login: .user.login, type: .user.type, at: .submitted_at, state: .state, commit_id: .commit_id, performed_via_github_app_id: .performed_via_github_app.id}",
    )?;
    let comments = read_records(
        run_gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        ".[] | {id: .id, login: .user.login, type: .user.type, at: .created_at, body: .body}",
    )?;
    Ok(decide_for_author(
        policy, head, &reviews, &comments, queue, false, author,
    ))
}

#[cfg(test)]
pub(crate) mod tests;
