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
//! ## Repositories that require review by default
//!
//! On a host that turns it on ([`host_review_default`]: `SHIPYARD_REVIEW_BEFORE_ARM=1`,
//! or a `review-before-arm` file in the guards directory),
//! [`REVIEW_REQUIRED_REPOS`] require a verdict naming the exact head even when
//! their base config is silent. Updating the binary changes nothing until a
//! host turns it on, so the rule rolls out one host at a time. A config that
//! sets `arm_requires_head_approval` itself still decides.
//!
//! With no reviewer listed, such a repository counts a verdict from the fleet
//! App (`shipyard-local`, which every agent posts as) or from an account whose
//! `author_association` is `OWNER`, `MEMBER` or `COLLABORATOR`; never from
//! other accounts, because the repositories are public, and never from a
//! distinguishable pull-request author. Listing `reviewer_identities` restores
//! the strict rule above. The queue-arm guard
//! (`scripts/ghapp_queue_arm_guard.py`) applies the same rule to every arm
//! and direct merge that goes through `ghapp`.
//!
//! ## Merge-only updates
//!
//! Updating an approved head with its base makes a new head no verdict names.
//! The approval carries to it ([`Approval::Carried`]) only when every commit
//! on the first-parent chain back to the approved head is a two-parent merge
//! whose merged parent is on the base, and every file the pull request
//! changes has the same blob at the new head as at the approved one. A file
//! the base also changed since, or any binary change in the pull request,
//! needs a new verdict.
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
/// Repositories that require an approval of the exact head when their base
/// config does not say. Kept equal to `REVIEW_REQUIRED_REPOS` in
/// `scripts/ghapp_queue_arm_guard.py` (a test reads both).
pub const REVIEW_REQUIRED_REPOS: [&str; 4] = [
    "Generous-Corp/pulp",
    "Generous-Corp/vellum",
    "danielraffel/tartci",
    "danielraffel/Shipyard",
];

/// Whether `repo` is one of [`REVIEW_REQUIRED_REPOS`].
#[must_use]
pub fn requires_review_by_default(repo: &str) -> bool {
    REVIEW_REQUIRED_REPOS
        .iter()
        .any(|required| required.eq_ignore_ascii_case(repo))
}

/// Environment switch for [`host_review_default`]: `1` on, `0` off.
pub const HOST_ENABLE_ENV: &str = "SHIPYARD_REVIEW_BEFORE_ARM";
/// File in the guards directory that turns [`host_review_default`] on.
pub const HOST_ENABLE_FILE: &str = "review-before-arm";
/// `author_association` values whose verdicts count when no reviewer is
/// listed (besides the fleet App's).
pub const TRUSTED_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

#[cfg(test)]
thread_local! {
    static HOST_DEFAULT_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Run `body` with [`host_review_default`] pinned, on this thread.
#[cfg(test)]
pub(crate) fn with_host_review_default<T>(enabled: bool, body: impl FnOnce() -> T) -> T {
    HOST_DEFAULT_OVERRIDE.with(|cell| cell.set(Some(enabled)));
    let result = body();
    HOST_DEFAULT_OVERRIDE.with(|cell| cell.set(None));
    result
}

#[cfg(test)]
fn host_review_override() -> Option<bool> {
    HOST_DEFAULT_OVERRIDE.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn host_review_override() -> Option<bool> {
    None
}

/// Whether this host applies [`REVIEW_REQUIRED_REPOS`]: `SHIPYARD_REVIEW_BEFORE_ARM`
/// (`1` or `0`) decides, else a `review-before-arm` file in the guards
/// directory (`$SHIPYARD_GHAPP_GUARDS_DIR`, else `~/.config/shipyard/guards`)
/// turns it on. Off otherwise.
#[must_use]
pub fn host_review_default() -> bool {
    if let Some(enabled) = host_review_override() {
        return enabled;
    }
    match std::env::var(HOST_ENABLE_ENV).as_deref() {
        Ok("1") => return true,
        Ok("0") => return false,
        _ => {}
    }
    let dir = std::env::var_os("SHIPYARD_GHAPP_GUARDS_DIR")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".config/shipyard/guards"))
        });
    dir.is_some_and(|dir| dir.join(HOST_ENABLE_FILE).is_file())
}

/// Removals of one head after which it needs a fresh approval to re-arm.
pub const EJECTION_CAP: usize = 2;

/// A `gh` transport. Returns stdout, or a message that includes stderr.
pub type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// The repository's approval policy.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ApprovalPolicy {
    /// Whether arming requires an approval of the exact head.
    pub required: bool,
    /// Identities whose exact-head approvals count, lower-cased.
    pub reviewer_logins: Vec<String>,
    /// Required by [`REVIEW_REQUIRED_REPOS`] rather than the repository's own
    /// setting: with no reviewer listed, the fleet App or a trusted
    /// association counts, never a distinguishable pull-request author.
    #[serde(default)]
    pub any_reviewer_but_author: bool,
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
            any_reviewer_but_author: false,
        }
    }

    /// [`Self::from_table`] for `repo`: on a host that turns the default on,
    /// a repository in [`REVIEW_REQUIRED_REPOS`] whose table does not set
    /// `arm_requires_head_approval` requires review.
    #[must_use]
    pub fn from_table_for_repo(table: &toml::Table, repo: &str, host_default: bool) -> Self {
        let mut policy = Self::from_table(table);
        let set = table
            .get(CONFIG_TABLE)
            .and_then(toml::Value::as_table)
            .is_some_and(|section| section.contains_key(REQUIRED_KEY));
        if !set && host_default && requires_review_by_default(repo) {
            policy.required = true;
            policy.any_reviewer_but_author = true;
        }
        policy
    }

    /// The policy of a repository whose base has no config file.
    #[must_use]
    pub fn without_config(repo: &str, host_default: bool) -> Self {
        Self::from_table_for_repo(&toml::Table::new(), repo, host_default)
    }

    /// Reject a policy that would let Shipyard approve its own PR.
    pub fn validate(&self) -> Result<(), String> {
        if self
            .reviewer_logins
            .iter()
            .any(|login| is_fleet_author(login))
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
    /// An approval of an earlier head, carried to this one by merge-only
    /// updates that left every file the pull request changes at its approved
    /// blob and mode.
    Carried {
        /// The head the approval names.
        from_head: String,
        /// The approval of that head.
        approval: Box<Approval>,
    },
    /// A `reviewed:<sha>` comment from an allowlisted reviewer.
    Marker {
        /// Commenter login.
        login: String,
        /// GitHub comment id used as durable approval evidence.
        #[serde(default)]
        comment_id: Option<u64>,
        /// GitHub App database id that authored the comment, when present.
        #[serde(default)]
        performed_via_github_app_id: Option<u64>,
    },
}

impl Approval {
    /// The comment that carries the approval, when it is a marker.
    #[must_use]
    pub fn comment_id(&self) -> Option<u64> {
        match self {
            Self::Marker { comment_id, .. } => *comment_id,
            Self::Review { .. } => None,
            Self::Carried { approval, .. } => approval.comment_id(),
        }
    }

    /// The GitHub App that posted the approval, when one did.
    #[must_use]
    pub fn app_id(&self) -> Option<u64> {
        match self {
            Self::Review {
                performed_via_github_app_id,
                ..
            }
            | Self::Marker {
                performed_via_github_app_id,
                ..
            } => *performed_via_github_app_id,
            Self::Carried { approval, .. } => approval.app_id(),
        }
    }

    /// One phrase naming who approved and how.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Review { login, .. } => format!("an approving review by {login}"),
            Self::Marker { login, .. } => format!("a reviewed:<sha> marker by {login}"),
            Self::Carried {
                from_head,
                approval,
            } => format!(
                "{} of {}, carried by merge-only updates with byte-identical own lines",
                approval.describe(),
                short(from_head)
            ),
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

/// `(approve, head)` from a comment body's verdict line: the same marker the
/// approval gate reads, for observers that only need to see verdicts.
pub(crate) fn verdict_line(body: &str) -> Option<(bool, String)> {
    marker_verdict(body).map(|(verdict, head)| (verdict == Verdict::Approve, head))
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

/// Whether a verdict by `login` (with its `author_association` when read)
/// counts when no reviewer is listed: the fleet App or a trusted association,
/// and never the distinguishable author of the pull request.
#[must_use]
pub(crate) fn trusted_reviewer(
    login: &str,
    association: Option<&str>,
    author: Option<&str>,
) -> bool {
    let trusted = is_fleet_author(login)
        || association.is_some_and(|association| TRUSTED_ASSOCIATIONS.contains(&association));
    trusted
        && author.is_none_or(|author| {
            normalized_login(author) != normalized_login(login) || is_fleet_author(author)
        })
}

/// Whether the verdict in `record` (`login`, and `association` when read)
/// counts under `policy` for a pull request by `author`.
fn reviewer_allowed(record: &Value, policy: &ApprovalPolicy, author: Option<&str>) -> bool {
    let Some(login) = login(record) else {
        return false;
    };
    let reviewer = normalized_login(login);
    if policy.any_reviewer_but_author && policy.reviewer_logins.is_empty() {
        // The repositories are public: only the fleet App and accounts the
        // repository trusts count. The fleet App is every agent's identity,
        // so its own pull request is not distinguishable from its reviewer's
        // comment.
        return trusted_reviewer(
            login,
            record.get("association").and_then(Value::as_str),
            author,
        );
    }
    !is_fleet_author(login)
        && policy
            .reviewer_logins
            .iter()
            .any(|identity| normalized_login(identity) == reviewer)
        && author.is_none_or(|author| normalized_login(author) != reviewer)
}

/// Return audit labels for reviewer text that was deliberately ignored.
///
/// These labels are kept separate from the gate decision so a stale HOLD or
/// an unstructured prose mention cannot accidentally block a newer head while
/// remaining visible in the enqueue receipt's evidence trail.
pub fn verdict_audit_events(
    head: &str,
    reviews: &[Value],
    comments: &[Value],
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> Vec<String> {
    let mut events = Vec::new();
    for comment in comments {
        if !reviewer_allowed(comment, policy, author) {
            continue;
        }
        let Some(body) = comment.get("body").and_then(Value::as_str) else {
            continue;
        };
        if let Some((Verdict::Hold, marker_head)) = marker_verdict(body)
            && !marker_head.eq_ignore_ascii_case(head)
        {
            events.push("stale_hold".to_owned());
        } else if marker_verdict(body).is_none()
            && body.lines().any(|line| {
                let upper = line.to_ascii_uppercase();
                upper.contains("HOLD") || upper.contains("CHANGES REQUESTED")
            })
        {
            events.push("unstructured_hold_mention".to_owned());
        }
    }
    for review in reviews {
        if !reviewer_allowed(review, policy, author) {
            continue;
        }
        let Some(state) = review.get("state").and_then(Value::as_str) else {
            continue;
        };
        if state.eq_ignore_ascii_case("CHANGES_REQUESTED")
            && review
                .get("commit_id")
                .and_then(Value::as_str)
                .is_some_and(|commit| !commit.eq_ignore_ascii_case(head))
        {
            events.push("stale_hold".to_owned());
        }
    }
    events
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
            && reviewer_allowed(review, policy, author)
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
                && reviewer_allowed(comment, policy, author)
                && comment
                    .get("body")
                    .and_then(Value::as_str)
                    .is_some_and(|body| marker_names_head(body, head))
        })
        .and_then(|comment| {
            Some(Approval::Marker {
                login: login(comment)?.to_owned(),
                comment_id: comment.get("id").and_then(Value::as_u64),
                performed_via_github_app_id: comment
                    .get("performed_via_github_app_id")
                    .and_then(Value::as_u64),
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
                "head {} carries no approval: it needs a comment whose last line is \
                 `verdict: APPROVE head:{head}` or `reviewed: {head}`, or an APPROVED review \
                 on that commit, from a listed reviewer (anyone but the PR author where none \
                 is listed), with no later HOLD naming it. This repository arms only approved \
                 heads; left disarmed. Arm deliberately with `--arm`",
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
        if !reviewer_allowed(review, policy, author)
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
        Some((at(review), verdict))
    });
    let comment_events = comments.iter().filter_map(|comment| {
        if !reviewer_allowed(comment, policy, author) {
            return None;
        }
        let (verdict, marker_head) = marker_verdict(comment.get("body")?.as_str()?)?;
        if marker_head != head.to_ascii_lowercase() {
            return None;
        }
        Some((at(comment), verdict))
    });
    let events: Vec<_> = review_events.chain(comment_events).collect();
    // A HOLD whose time cannot be read cannot be ordered against an approval,
    // so it is not allowed to lose to one.
    if events
        .iter()
        .any(|(timestamp, verdict)| timestamp.is_none() && *verdict == Verdict::Hold)
    {
        return Some(Verdict::Hold);
    }
    events
        .into_iter()
        .filter_map(|(timestamp, verdict)| Some((timestamp?, verdict)))
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
            let host_default = host_review_default();
            if host_default && requires_review_by_default(repo) {
                return Ok(ApprovalPolicy::without_config(repo, host_default));
            }
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
    let policy = ApprovalPolicy::from_table_for_repo(&table, repo, host_review_default());
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

/// A review as the gate reads it.
const REVIEW_RECORD_JQ: &str = ".[] | {login: .user.login, type: .user.type, association: .author_association, at: .submitted_at, state: .state, commit_id: .commit_id, performed_via_github_app_id: .performed_via_github_app.id}";
/// A comment as the gate reads it.
const COMMENT_RECORD_JQ: &str = ".[] | {id: .id, login: .user.login, type: .user.type, association: .author_association, at: .created_at, body: .body, performed_via_github_app_id: .performed_via_github_app.id}";

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
    let author = pr_author(run_gh, repo, pr, &policy, None)?;
    let reviews = read_records(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews"),
        REVIEW_RECORD_JQ,
    )?;
    let comments = read_records(
        run_gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        COMMENT_RECORD_JQ,
    )?;
    let gate = decide_for_author(
        &policy,
        head,
        &reviews,
        &comments,
        queue,
        latest_was_interruption,
        author.as_deref(),
    );
    Ok(with_carry(
        run_gh,
        &CarryInput {
            repo,
            pr,
            head,
            reviews: &reviews,
            comments: &comments,
            policy: &policy,
            author: author.as_deref(),
        },
        gate,
    ))
}

/// The pull request's author, read when the policy turns on it: with no
/// reviewer listed, a verdict counts only from someone other than the author.
fn pr_author(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    policy: &ApprovalPolicy,
    known: Option<&str>,
) -> Result<Option<String>, String> {
    if known.is_some() || !policy.any_reviewer_but_author || !policy.reviewer_logins.is_empty() {
        return Ok(known.map(str::to_owned));
    }
    let login = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        ".user.login".to_owned(),
    ])?;
    let login = login.trim();
    if login.is_empty() {
        return Err(format!("PR #{pr} carried no author"));
    }
    Ok(Some(login.to_owned()))
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
    evaluate_with_policy_for_author_with_events(run_gh, repo, pr, head, policy, queue, author)
        .map(|(gate, _)| gate)
}

/// Evaluate and return ignored verdict evidence for the mutation receipt.
pub fn evaluate_with_policy_for_author_with_events(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    policy: &ApprovalPolicy,
    queue: &Value,
    author: Option<&str>,
) -> Result<(HeadGate, Vec<String>), String> {
    if !policy.required {
        return Ok((HeadGate::NotRequired, Vec::new()));
    }
    let author = pr_author(run_gh, repo, pr, policy, author)?;
    let author = author.as_deref();
    let reviews = read_records(
        run_gh,
        &format!("repos/{repo}/pulls/{pr}/reviews"),
        REVIEW_RECORD_JQ,
    )?;
    let comments = read_records(
        run_gh,
        &format!("repos/{repo}/issues/{pr}/comments"),
        COMMENT_RECORD_JQ,
    )?;
    let comments = if comments.iter().any(|comment| {
        comment
            .get("body")
            .and_then(Value::as_str)
            .and_then(marker_verdict)
            .is_some_and(|(_, marker_head)| marker_head.eq_ignore_ascii_case(head))
    }) {
        let created_at = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/commits/{head}"),
            "--jq".to_owned(),
            ".commit.committer.date".to_owned(),
        ])?
        .trim()
        .parse::<DateTime<Utc>>()
        .map_err(|error| format!("head commit timestamp unreadable: {error}"))?;
        comments
            .into_iter()
            .filter(|comment| at(comment).is_some_and(|comment_at| comment_at > created_at))
            .collect()
    } else {
        comments
    };
    let events = verdict_audit_events(head, &reviews, &comments, policy, author);
    let gate = decide_for_author(policy, head, &reviews, &comments, queue, false, author);
    let carry = CarryInput {
        repo,
        pr,
        head,
        reviews: &reviews,
        comments: &comments,
        policy,
        author,
    };
    Ok((with_carry(run_gh, &carry, gate), events))
}

/// What [`with_carry`] reads about one pull request.
struct CarryInput<'a> {
    repo: &'a str,
    pr: u64,
    head: &'a str,
    reviews: &'a [Value],
    comments: &'a [Value],
    policy: &'a ApprovalPolicy,
    author: Option<&'a str>,
}

/// Heads a counting verdict names, newest verdict first.
fn verdict_heads(input: &CarryInput<'_>) -> Vec<String> {
    let mut named: Vec<(Option<DateTime<Utc>>, String)> = Vec::new();
    for review in input.reviews {
        if reviewer_allowed(review, input.policy, input.author)
            && let Some(commit) = review.get("commit_id").and_then(Value::as_str)
        {
            named.push((at(review), commit.to_ascii_lowercase()));
        }
    }
    for comment in input.comments {
        if reviewer_allowed(comment, input.policy, input.author)
            && let Some((_, head)) = comment
                .get("body")
                .and_then(Value::as_str)
                .and_then(marker_verdict)
        {
            named.push((at(comment), head));
        }
    }
    named.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut heads: Vec<String> = Vec::new();
    for (_, head) in named {
        if !heads.contains(&head) {
            heads.push(head);
        }
    }
    heads
}

/// `gate`, or an approval carried from an earlier head when no verdict names
/// this one and the newest approved earlier head reaches it by merge-only
/// updates with byte-identical own lines. Anything unreadable leaves `gate`.
fn with_carry(run_gh: RunGh<'_>, input: &CarryInput<'_>, gate: HeadGate) -> HeadGate {
    if !matches!(gate, HeadGate::NotApproved { .. }) {
        return gate;
    }
    let verdict = |head: &str| {
        latest_verdict(
            head,
            input.reviews,
            input.comments,
            input.policy,
            input.author,
        )
    };
    if verdict(input.head).is_some() {
        // A verdict names this head; only a newer verdict can change it.
        return gate;
    }
    let Some(from) = verdict_heads(input)
        .into_iter()
        .find(|head| !head.eq_ignore_ascii_case(input.head))
        .filter(|head| verdict(head) == Some(Verdict::Approve))
    else {
        return gate;
    };
    let Some(approval) = find_approval_for_author(
        &from,
        input.reviews,
        input.comments,
        input.policy,
        None,
        input.author,
    ) else {
        return gate;
    };
    match merge_only_carry(run_gh, input.repo, input.pr, &from, input.head) {
        Ok(()) => HeadGate::Approved {
            approval: Approval::Carried {
                from_head: from,
                approval: Box::new(approval),
            },
        },
        Err(_) => gate,
    }
}

fn gh_json(run_gh: RunGh<'_>, args: &[String]) -> Result<Value, String> {
    serde_json::from_str(run_gh(args)?.trim()).map_err(|error| format!("invalid JSON: {error}"))
}

/// Git calls a file binary when a NUL byte appears in its first 8000 bytes.
const BINARY_PROBE_BYTES: usize = 8000;

/// Whether blob `sha` is binary by git's rule. GitHub omits a comparison's
/// `patch` for a binary file but also for a text file too large to show (a
/// one-line JSON fixture of hundreds of KB), so a missing patch alone does not
/// say binary. An unreadable blob is an error: unknown refuses, but is never
/// reported as binary.
fn blob_is_binary(run_gh: RunGh<'_>, repo: &str, sha: &str) -> Result<bool, String> {
    let blob = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/git/blobs/{sha}"),
            "--jq".to_owned(),
            "{encoding, content, size}".to_owned(),
        ],
    )
    .map_err(|error| format!("cannot tell whether blob {} is binary: {error}", short(sha)))?;
    binary_blob(&blob)
        .map_err(|error| format!("cannot tell whether blob {} is binary: {error}", short(sha)))
}

/// Whether a `git/blobs` answer is binary by git's rule. Its decoded content
/// must be exactly its reported `size`: truncated or empty content is
/// unknown, never text.
fn binary_blob(blob: &Value) -> Result<bool, String> {
    use base64::Engine as _;
    let (Some("base64"), Some(content), Some(size)) = (
        blob.get("encoding").and_then(Value::as_str),
        blob.get("content").and_then(Value::as_str),
        blob.get("size").and_then(Value::as_u64),
    ) else {
        return Err("it is unreadable".to_owned());
    };
    let compact: String = content.chars().filter(|ch| !ch.is_whitespace()).collect();
    let data = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .map_err(|error| error.to_string())?;
    if data.len() as u64 != size {
        return Err(format!("{} bytes read of {size}", data.len()));
    }
    Ok(data.iter().take(BINARY_PROBE_BYTES).any(|byte| *byte == 0))
}

/// GitHub's compare lists at most this many files; a longer list is partial.
const COMPARE_FILE_CAP: usize = 300;

/// `filename -> (status, blob sha, has a text patch)` of one comparison, its
/// merge base, and `filename -> previous_filename` for every rename in it.
type PrFiles = (
    std::collections::BTreeMap<String, (String, String, bool)>,
    String,
    std::collections::BTreeMap<String, String>,
);

fn compare_files(run_gh: RunGh<'_>, repo: &str, range: &str) -> Result<PrFiles, String> {
    let value = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/compare/{range}"),
            "--jq".to_owned(),
            "{merge_base: .merge_base_commit.sha, files: [.files[] | {filename, status, sha, \
             previous_filename, patch: (.patch != null)}]}"
                .to_owned(),
        ],
    )?;
    let files = value
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("compare/{range} lists no files"))?;
    if files.len() >= COMPARE_FILE_CAP {
        return Err(format!(
            "compare/{range} lists {} files, GitHub's cap; it may be partial",
            files.len()
        ));
    }
    let mut out = std::collections::BTreeMap::new();
    let mut sources = std::collections::BTreeMap::new();
    for file in files {
        let text = |key: &str| file.get(key).and_then(Value::as_str).map(str::to_owned);
        let name =
            text("filename").ok_or_else(|| format!("compare/{range}: a file has no name"))?;
        if let Some(previous) = text("previous_filename") {
            sources.insert(name.clone(), previous);
        }
        out.insert(
            name,
            (
                text("status").unwrap_or_default(),
                text("sha").unwrap_or_default(),
                file.get("patch").and_then(Value::as_bool) == Some(true),
            ),
        );
    }
    let base = value
        .get("merge_base")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("compare/{range}: no merge base"))?
        .to_owned();
    Ok((out, base, sources))
}

/// The first of the pull request's files the base touched between two merge
/// bases, a rename counting under both names on either side: a base edit to a
/// file the pull request renames from would be dropped by a merge that keeps
/// the approved destination.
fn touched_by_base(
    before: &std::collections::BTreeMap<String, (String, String, bool)>,
    pr_sources: &[&std::collections::BTreeMap<String, String>],
    moved: &std::collections::BTreeMap<String, (String, String, bool)>,
    moved_sources: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    let touched: std::collections::BTreeSet<&str> = moved
        .keys()
        .map(String::as_str)
        .chain(moved_sources.values().map(String::as_str))
        .collect();
    before
        .keys()
        .map(String::as_str)
        .chain(
            pr_sources
                .iter()
                .flat_map(|sources| sources.values().map(String::as_str)),
        )
        .filter(|name| touched.contains(name))
        .min()
        .map(str::to_owned)
}

/// The first file whose rename source differs between two comparisons.
fn rename_source_moved(
    before: &std::collections::BTreeMap<String, String>,
    after: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    before
        .keys()
        .chain(after.keys())
        .find(|name| before.get(*name) != after.get(*name))
        .cloned()
}

/// Whether the approval of `from` carries to `to`.
///
/// It carries only when every commit on the first-parent chain from `to`
/// back to `from` is a two-parent merge whose merged parent is on the base,
/// and every file the pull request changes has, at `to`, the same blob and
/// mode it had at `from`. A file the base also changed since `from`, or any binary
/// change in the pull request, needs a new verdict: a merge could move or
/// swap content that a diff of the changed lines cannot see.
///
/// # Errors
///
/// Why the approval of `from` does not carry to `to`.
pub fn merge_only_carry(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    from: &str,
    to: &str,
) -> Result<(), String> {
    let base = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}"),
        "--jq".to_owned(),
        ".base.ref".to_owned(),
    ])?
    .trim()
    .to_owned();
    if base.is_empty() {
        return Err("the pull request has no base branch".to_owned());
    }
    first_parent_merges(run_gh, repo, &base, from, to)?;
    let (before, base_before, before_sources) =
        compare_files(run_gh, repo, &format!("{base}...{from}"))?;
    let (after, base_after, after_sources) =
        compare_files(run_gh, repo, &format!("{base}...{to}"))?;
    for (name, (_, sha, patch)) in &before {
        if !patch && blob_is_binary(run_gh, repo, sha)? {
            return Err(format!("the pull request changes a binary file ({name})"));
        }
    }
    if let Some(name) = before
        .keys()
        .find(|name| !after.contains_key(*name))
        .or_else(|| after.keys().find(|name| !before.contains_key(*name)))
    {
        return Err(format!(
            "the set of files the pull request changes moved ({name})"
        ));
    }
    for (name, (status, sha, _)) in &before {
        if after
            .get(name)
            .is_none_or(|(after_status, after_sha, _)| after_status != status || after_sha != sha)
        {
            return Err(format!(
                "{name} at {} is not the blob approved at {}",
                short(to),
                short(from)
            ));
        }
    }
    if let Some(name) = rename_source_moved(&before_sources, &after_sources) {
        return Err(format!(
            "{name} is renamed from {} at {}, from {} when approved",
            after_sources.get(&name).map_or("nothing", String::as_str),
            short(to),
            before_sources.get(&name).map_or("nothing", String::as_str),
        ));
    }
    // The comparison carries no file mode, so a merge that only flips a
    // file's executable bit keeps its blob: read the modes themselves.
    let present: Vec<&str> = before
        .iter()
        .filter(|(_, (status, _, _))| status != "removed")
        .map(|(name, _)| name.as_str())
        .collect();
    if !present.is_empty() {
        let approved_modes = tree_modes(run_gh, repo, from, &present)?;
        let head_modes = tree_modes(run_gh, repo, to, &present)?;
        if let Some(name) = present
            .iter()
            .find(|name| approved_modes.get(**name) != head_modes.get(**name))
        {
            return Err(format!(
                "{name} at {} does not keep the mode approved at {}",
                short(to),
                short(from)
            ));
        }
    }
    if base_before != base_after {
        let (moved, _, moved_sources) =
            compare_files(run_gh, repo, &format!("{base_before}...{base_after}"))?;
        if let Some(name) = touched_by_base(
            &before,
            &[&before_sources, &after_sources],
            &moved,
            &moved_sources,
        ) {
            return Err(format!(
                "{name} was changed by both the pull request and the base"
            ));
        }
    }
    Ok(())
}

/// Directories read per GraphQL query by [`tree_modes`].
const TREE_BATCH: usize = 50;

/// Each of `names`' git mode at `commit` (absent when the file is), read from
/// its directory's tree.
fn tree_modes(
    run_gh: RunGh<'_>,
    repo: &str,
    commit: &str,
    names: &[&str],
) -> Result<std::collections::BTreeMap<String, u64>, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("invalid repository {repo:?}"))?;
    let mut by_dir: std::collections::BTreeMap<&str, Vec<&str>> = std::collections::BTreeMap::new();
    for path in names {
        let directory = path.rsplit_once('/').map_or("", |(directory, _)| directory);
        by_dir.entry(directory).or_default().push(path);
    }
    let directories: Vec<&str> = by_dir.keys().copied().collect();
    let quote = |text: &str| Value::String(text.to_owned()).to_string();
    let mut modes = std::collections::BTreeMap::new();
    for batch in directories.chunks(TREE_BATCH) {
        let mut fields = String::new();
        for (index, directory) in batch.iter().enumerate() {
            let _ = std::fmt::Write::write_fmt(
                &mut fields,
                format_args!(
                    "d{index}:object(expression:{}){{...on Tree{{entries{{name mode}}}}}}",
                    quote(&format!("{commit}:{directory}"))
                ),
            );
        }
        let query = format!(
            "query{{repository(owner:{},name:{}){{{fields}}}}}",
            quote(owner),
            quote(name)
        );
        let value = gh_json(
            run_gh,
            &[
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={query}"),
            ],
        )?;
        if value.get("errors").is_some() {
            return Err(format!("the trees at {} are unreadable", short(commit)));
        }
        let repository = value
            .pointer("/data/repository")
            .and_then(Value::as_object)
            .ok_or_else(|| format!("the trees at {} are unreadable", short(commit)))?;
        for (index, directory) in batch.iter().enumerate() {
            let entries = repository
                .get(&format!("d{index}"))
                .and_then(|tree| tree.get("entries"))
                .and_then(Value::as_array);
            for path in &by_dir[directory] {
                let base = path.rsplit_once('/').map_or(*path, |(_, base)| base);
                let mode = entries.into_iter().flatten().find_map(|entry| {
                    (entry.get("name").and_then(Value::as_str) == Some(base))
                        .then(|| entry.get("mode").and_then(Value::as_u64))
                        .flatten()
                });
                if let Some(mode) = mode {
                    modes.insert((*path).to_owned(), mode);
                }
            }
        }
    }
    Ok(modes)
}

/// Every commit on the first-parent chain from `to` back to `from` is a
/// two-parent merge whose merged parent is on `base`. The other commits in
/// the range arrived through those merged parents.
fn first_parent_merges(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    from: &str,
    to: &str,
) -> Result<(), String> {
    let range = gh_json(
        run_gh,
        &[
            "api".to_owned(),
            format!("repos/{repo}/compare/{from}...{to}"),
            "--jq".to_owned(),
            "{status, behind_by, total_commits, commits: [.commits[] | {sha, parents: [.parents[].sha]}]}"
                .to_owned(),
        ],
    )?;
    if range.get("status").and_then(Value::as_str) != Some("ahead")
        || range.get("behind_by").and_then(Value::as_u64) != Some(0)
    {
        return Err(format!(
            "{} does not descend from {}",
            short(to),
            short(from)
        ));
    }
    let commits = range
        .get("commits")
        .and_then(Value::as_array)
        .ok_or("the comparison lists no commits")?;
    if Some(commits.len() as u64) != range.get("total_commits").and_then(Value::as_u64) {
        return Err("the comparison lists only some of its commits".to_owned());
    }
    let parents_of: std::collections::BTreeMap<String, Vec<String>> = commits
        .iter()
        .filter_map(|commit| {
            let sha = commit.get("sha")?.as_str()?.to_ascii_lowercase();
            let parents = commit
                .get("parents")?
                .as_array()?
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect();
            Some((sha, parents))
        })
        .collect();
    let mut sha = to.to_ascii_lowercase();
    while sha != from.to_ascii_lowercase() {
        let parents = parents_of
            .get(&sha)
            .ok_or_else(|| format!("commit {} is outside the comparison", short(&sha)))?;
        let [first, merged] = parents.as_slice() else {
            return Err(format!("commit {} is not a merge", short(&sha)));
        };
        let on_base = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/compare/{merged}...{base}"),
            "--jq".to_owned(),
            ".status".to_owned(),
        ])?;
        if !matches!(on_base.trim(), "ahead" | "identical") {
            return Err(format!("merged parent {} is not on {base}", short(merged)));
        }
        sha.clone_from(first);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
