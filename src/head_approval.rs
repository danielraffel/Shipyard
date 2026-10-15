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
//! Unless the host opted out ([`host_review_default`]: a
//! `review-before-arm.opt-out` file in the guards directory, the only way
//! off), [`REVIEW_REQUIRED_REPOS`] require a verdict naming the exact head
//! even when their base config is silent. A config that sets
//! `arm_requires_head_approval` itself still decides.
//!
//! An approval does not count for the agent that posted it: `ghapp` stamps
//! verdict comments with `<!-- session: <host>/<session> -->`, plus
//! `agent: <id>` when [`AGENT_ENV`] is set, and a marker approval stamped
//! with this process's agent id is [`HeadGate::SelfApproved`]. The session
//! alone never refuses: every agent of one Claude Code team shares
//! `CLAUDE_CODE_SESSION_ID`, so it cannot tell the reviewer from the agent
//! that arms. Unstamped verdicts, and either side without an agent id,
//! count as before.
//!
//! With no reviewer listed, such a repository counts an approval only from
//! the fleet App (`shipyard-local`, which every agent posts as through
//! `ghapp`), or from a human account the base lists in `[auto_merge]
//! human_reviewers` (empty by default) whose `author_association` is `OWNER`,
//! `MEMBER` or `COLLABORATOR`. A human approval outside that list does not
//! count, because any agent holding the owner's own `gh` login could post one;
//! each one excluded is reported (`excluded_human_approval:<login>`). A HOLD
//! from such an account still counts: it can only stop a landing. Never from
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

mod rebase;
pub use rebase::{
    CarryRecord, REBASE_CARRY_ENV, REBASE_CARRY_FILE, REBASE_CARRY_LOG, patch_id, rebase_carry,
    rebase_carry_enabled,
};

/// Config table holding the settings this module reads.
pub const CONFIG_TABLE: &str = "auto_merge";
/// Key turning the check on.
pub const REQUIRED_KEY: &str = "arm_requires_head_approval";
/// Key listing logins whose `reviewed:<sha>` comments count as approval.
pub const REVIEWERS_KEY: &str = "reviewer_logins";
/// New spelling for the configured reviewer identities.
pub const REVIEWER_IDENTITIES_KEY: &str = "reviewer_identities";
/// Human accounts whose approvals count when no reviewer is listed. Empty by
/// default: then only the fleet App's verdicts approve.
pub const HUMAN_REVIEWERS_KEY: &str = "human_reviewers";
/// Who may sign the fleet App's APPROVE, set by the protected base; it takes
/// precedence over a host's [`SIGNERS_FILE`].
pub const SIGNERS_KEY: &str = "reviewer_signers";
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

/// Environment switch for [`host_review_default`]: `1` forces it on for one
/// process. Nothing in the environment turns it off.
pub const HOST_ENABLE_ENV: &str = "SHIPYARD_REVIEW_BEFORE_ARM";
/// File in the guards directory that turns [`host_review_default`] off.
pub const HOST_OPT_OUT_FILE: &str = "review-before-arm.opt-out";
/// Per-agent identity the orchestrator sets for each agent. Only it can
/// refuse a self-approval; see [`HeadGate::SelfApproved`].
pub const AGENT_ENV: &str = "SHIPYARD_AGENT_ID";
/// Where an agent session's id is read from, in order. Shared by every agent
/// of one team, so it attributes but never refuses.
pub const SESSION_ENVS: [&str; 2] = ["CLAUDE_CODE_SESSION_ID", "CODEX_COMPANION_SESSION_ID"];
/// `author_association` values whose verdicts count when no reviewer is
/// listed (besides the fleet App's).
pub const TRUSTED_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

#[cfg(test)]
thread_local! {
    static HOST_DEFAULT_OVERRIDE: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
    static AGENT_PINNED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static AGENT_OVERRIDE: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
    static HOST_SIGNERS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Run `body` as if this host's signers file allowed `signers`, on this thread.
#[cfg(test)]
pub(crate) fn with_host_signers<T>(signers: &[&str], body: impl FnOnce() -> T) -> T {
    HOST_SIGNERS.with(|cell| {
        *cell.borrow_mut() = signers.iter().map(|name| (*name).to_owned()).collect();
    });
    let result = body();
    HOST_SIGNERS.with(|cell| cell.borrow_mut().clear());
    result
}

/// Run `body` with [`arming_agent`] pinned, on this thread.
#[cfg(test)]
pub(crate) fn with_arming_agent<T>(agent: Option<&str>, body: impl FnOnce() -> T) -> T {
    AGENT_PINNED.with(|cell| cell.set(true));
    AGENT_OVERRIDE.with(|cell| *cell.borrow_mut() = agent.map(str::to_owned));
    let result = body();
    AGENT_PINNED.with(|cell| cell.set(false));
    AGENT_OVERRIDE.with(|cell| *cell.borrow_mut() = None);
    result
}

#[cfg(test)]
fn arming_agent_pinned() -> bool {
    AGENT_PINNED.with(std::cell::Cell::get)
}

#[cfg(not(test))]
const fn arming_agent_pinned() -> bool {
    false
}

#[cfg(test)]
fn pinned_arming_agent() -> Option<String> {
    AGENT_OVERRIDE.with(|cell| cell.borrow().clone())
}

#[cfg(not(test))]
const fn pinned_arming_agent() -> Option<String> {
    None
}

/// This process's agent id ([`AGENT_ENV`]), when the orchestrator set one.
#[must_use]
pub fn arming_agent() -> Option<String> {
    if arming_agent_pinned() {
        return pinned_arming_agent();
    }
    std::env::var(AGENT_ENV)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn stamp_pattern() -> Regex {
    Regex::new(r"<!--\s*session:\s*([^/\s]+)/(\S+?)(?:\s+agent:\s*(\S+?))?\s*-->")
        .expect("stamp pattern compiles")
}

/// The session a verdict comment was stamped with by `ghapp`, if any.
#[must_use]
pub fn stamped_session(body: &str) -> Option<String> {
    stamp_pattern()
        .captures_iter(body)
        .last()
        .and_then(|capture| capture.get(2))
        .map(|session| session.as_str().to_owned())
}

/// The agent id a verdict comment was stamped with by `ghapp`, if any.
#[must_use]
pub fn stamped_agent(body: &str) -> Option<String> {
    stamp_pattern()
        .captures_iter(body)
        .last()
        .and_then(|capture| capture.get(3))
        .map(|agent| agent.as_str().to_owned())
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

/// What sits at a [`HOST_OPT_OUT_FILE`] path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptOut {
    /// Nothing (a dangling symlink included): review before arm is on.
    Absent,
    /// A regular file holding the JSON `shipyard guards install
    /// --opt-out-review-before-arm` writes, with a non-empty `reason`, `at`
    /// and `by`: the only thing that turns review before arm off.
    Valid {
        /// The recorded reason.
        reason: String,
        /// The record as written.
        record: String,
    },
    /// Anything else: a directory, an empty or hand-made file, garbage.
    /// Review before arm stays on, and `guards status` fails until it is
    /// fixed or removed.
    Invalid {
        /// Why it does not count.
        detail: String,
    },
}

/// Classify the opt-out at `path`. The Python twin
/// (`ghapp_queue_arm_guard.valid_opt_out`) applies the same rules.
#[must_use]
pub fn read_opt_out(path: &std::path::Path) -> OptOut {
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return OptOut::Absent,
        Err(error) => {
            return OptOut::Invalid {
                detail: error.to_string(),
            };
        }
        Ok(meta) if !meta.is_file() => {
            return OptOut::Invalid {
                detail: "not a regular file".to_owned(),
            };
        }
        Ok(_) => {}
    }
    let record = match std::fs::read_to_string(path) {
        Ok(record) => record.trim().to_owned(),
        Err(error) => {
            return OptOut::Invalid {
                detail: error.to_string(),
            };
        }
    };
    let parsed = serde_json::from_str::<Value>(&record).ok();
    let field = |key: &str| {
        parsed
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    };
    match (field("reason"), field("at"), field("by")) {
        (Some(reason), Some(_), Some(_)) => OptOut::Valid { reason, record },
        _ => OptOut::Invalid {
            detail: "not an opt-out record (needs JSON with reason, at and by)".to_owned(),
        },
    }
}

/// Whether this host applies [`REVIEW_REQUIRED_REPOS`]: on, unless the
/// guards directory (`$SHIPYARD_GHAPP_GUARDS_DIR`, else
/// `~/.config/shipyard/guards`) holds a valid [`HOST_OPT_OUT_FILE`] (see
/// [`read_opt_out`]). `SHIPYARD_REVIEW_BEFORE_ARM=1` forces it on.
#[must_use]
pub fn host_review_default() -> bool {
    if let Some(enabled) = host_review_override() {
        return enabled;
    }
    if std::env::var(HOST_ENABLE_ENV).as_deref() == Ok("1") {
        return true;
    }
    !guards_dir().is_some_and(|dir| {
        matches!(
            read_opt_out(&dir.join(HOST_OPT_OUT_FILE)),
            OptOut::Valid { .. }
        )
    })
}

/// The guards directory: `$SHIPYARD_GHAPP_GUARDS_DIR`, else
/// `~/.config/shipyard/guards`.
pub(crate) fn guards_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("SHIPYARD_GHAPP_GUARDS_DIR")
        .filter(|dir| !dir.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| std::path::PathBuf::from(home).join(".config/shipyard/guards"))
        })
}

/// Guards-directory file naming who may sign the fleet App's APPROVE:
/// `{"default": ["pulp-review", ...], "repos": {"OWNER/REPO": [...]}}`. The
/// queue-arm guard reads the same file.
pub const SIGNERS_FILE: &str = "reviewer-signers.json";

/// The reviewer names that may sign an App APPROVE in `repo` on this host,
/// or `None` when the host has no signers file (the rule is off). A file
/// that cannot be read admits nobody.
#[must_use]
pub fn reviewer_signers(repo: &str) -> Option<Vec<String>> {
    signers_at(&guards_dir()?.join(SIGNERS_FILE), repo)
}

/// [`reviewer_signers`] read from `path`: `None` when there is no file,
/// nobody when it cannot be read or parsed.
fn signers_at(path: &std::path::Path, repo: &str) -> Option<Vec<String>> {
    if !path.exists() {
        return None;
    }
    Some(
        std::fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map_or_else(Vec::new, |data| signers_from(&data, repo)),
    )
}

/// The signers a [`SIGNERS_FILE`]'s contents name for `repo`: its own entry,
/// else `default`; nobody when neither is a list.
fn signers_from(data: &Value, repo: &str) -> Vec<String> {
    data.get("repos")
        .and_then(Value::as_object)
        .and_then(|repos| {
            repos
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(repo))
                .map(|(_, names)| names)
        })
        .or_else(|| data.get("default"))
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_ascii_lowercase)
                .collect()
        })
        .unwrap_or_default()
}

/// This host's [`reviewer_signers`] for `repo`. Tests read no host state.
#[cfg(not(test))]
fn host_signers(repo: &str) -> Option<Vec<String>> {
    reviewer_signers(repo)
}

#[cfg(test)]
fn host_signers(_repo: &str) -> Option<Vec<String>> {
    HOST_SIGNERS.with(|cell| {
        let signers = cell.borrow();
        (!signers.is_empty()).then(|| signers.clone())
    })
}

/// Whether this evaluation may try the rebase carry: the host switch
/// ([`rebase_carry_enabled`]). Tests read no host state.
#[cfg(not(test))]
fn host_rebase_carry() -> bool {
    rebase_carry_enabled()
}

#[cfg(test)]
const fn host_rebase_carry() -> bool {
    false
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
    /// Human logins whose approvals count under `any_reviewer_but_author`
    /// ([`HUMAN_REVIEWERS_KEY`]), lower-cased. Empty by default.
    #[serde(default)]
    pub human_reviewer_logins: Vec<String>,
    /// Who may sign the fleet App's APPROVE ([`reviewer_signers`]), lower-cased;
    /// `None` when this host has no signers file.
    #[serde(default)]
    pub signers: Option<Vec<String>>,
    /// Try the rebase carry for this evaluation whatever the host switch
    /// says (`shipyard review-verdict` sets it; a config never does).
    #[serde(default)]
    pub rebase_carry: bool,
    /// Where [`Self::signers`] came from. [`SignerSource::Base`] is the
    /// repository-side rule ([`read_policy_repository_side`]): only the
    /// base's `[auto_merge] reviewer_signers`, never a host, and with none
    /// listed an App APPROVE does not count.
    #[serde(default)]
    pub signer_source: SignerSource,
}

/// Where an [`ApprovalPolicy`]'s signers came from.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SignerSource {
    /// The base's list, else this host's signers file; neither leaves the
    /// signer rule off.
    #[default]
    Host,
    /// The base's list only; none listed refuses every App APPROVE.
    Base,
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
            human_reviewer_logins: section
                .get(HUMAN_REVIEWERS_KEY)
                .and_then(toml::Value::as_array)
                .map(|logins| {
                    logins
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(normalized_login)
                        .collect()
                })
                .unwrap_or_default(),
            signers: section
                .get(SIGNERS_KEY)
                .and_then(toml::Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_ascii_lowercase)
                        .collect()
                }),
            rebase_carry: false,
            signer_source: SignerSource::Host,
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
    /// updates, or across a rebase ([`rebase_carry`]), that left every file
    /// the pull request changes at its approved blob and mode.
    Carried {
        /// The head the approval names.
        from_head: String,
        /// The approval of that head.
        approval: Box<Approval>,
        /// Carried across a rebase rather than by merge-only updates.
        #[serde(default)]
        rebase: bool,
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
                rebase,
            } => format!(
                "{} of {}, carried {} with byte-identical own lines",
                approval.describe(),
                short(from_head),
                if *rebase {
                    "across a rebase (same patch-id, the base touched none of its files)"
                } else {
                    "by merge-only updates"
                }
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
    if is_fleet_author(login) && record.get("type").and_then(Value::as_str) != Some("Bot") {
        // Only the App itself is a Bot; a user account named like it is
        // anyone at all.
        return false;
    }
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

/// Whether an approval by `record`'s login counts, beyond
/// [`reviewer_allowed`]: under the default rule (no reviewer listed) only the
/// fleet App or a listed human reviewer approves. A repository that lists
/// `reviewer_identities` decides by that list alone.
fn approver_allowed(record: &Value, policy: &ApprovalPolicy) -> bool {
    if !(policy.any_reviewer_but_author && policy.reviewer_logins.is_empty()) {
        return true;
    }
    login(record).is_some_and(|login| {
        if is_fleet_author(login) {
            return app_approval_refusal(record, policy).is_none();
        }
        policy
            .human_reviewer_logins
            .iter()
            .any(|human| *human == normalized_login(login))
    })
}

/// Why the fleet App's APPROVE in `record` does not count, or `None`.
///
/// Every agent posts as the same App, so the App's identity says nothing
/// about who reviewed: an APPROVE `ghapp` stamped with an `unattributed`
/// session never counts, and on a host with a signers file the body must
/// carry a `reviewer: <name>` line naming an allowed signer.
fn app_approval_refusal(record: &Value, policy: &ApprovalPolicy) -> Option<String> {
    let body = record
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stamped_session(body).as_deref() == Some("unattributed") {
        return Some("its session stamp is unattributed".to_owned());
    }
    if policy.signer_source == SignerSource::Base && policy.signers.is_none() {
        return Some(format!(
            "no signer list is configured on the base branch (`[{CONFIG_TABLE}] {SIGNERS_KEY}`)"
        ));
    }
    let signers = policy.signers.as_ref()?;
    let pattern = Regex::new(r"(?m)^[ \t]*reviewer:[ \t]*([A-Za-z0-9._-]+)[ \t]*$")
        .expect("signer pattern compiles");
    let signed: Vec<String> = pattern
        .captures_iter(body)
        .filter_map(|capture| capture.get(1))
        .map(|name| name.as_str().to_ascii_lowercase())
        .collect();
    if signed.is_empty() {
        return Some("it has no `reviewer: <name>` line".to_owned());
    }
    if !signed.iter().any(|name| signers.contains(name)) {
        return Some(format!(
            "its signer {} is not one of {}",
            signed.last().map_or("", String::as_str),
            if signers.is_empty() {
                "nobody".to_owned()
            } else {
                signers.join(", ")
            }
        ));
    }
    None
}

/// Whether `record`'s `verdict` counts under `policy`: a HOLD from any
/// counting reviewer, an approval only from an allowed approver.
fn verdict_counts(
    record: &Value,
    verdict: Verdict,
    policy: &ApprovalPolicy,
    author: Option<&str>,
) -> bool {
    reviewer_allowed(record, policy, author)
        && (verdict == Verdict::Hold || approver_allowed(record, policy))
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
    for record in reviews.iter().chain(comments) {
        let approves = record
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| state.eq_ignore_ascii_case("APPROVED"))
            || record
                .get("body")
                .and_then(Value::as_str)
                .and_then(marker_verdict)
                .is_some_and(|(verdict, _)| verdict == Verdict::Approve);
        if approves
            && reviewer_allowed(record, policy, author)
            && !approver_allowed(record, policy)
            && let Some(login) = login(record)
        {
            events.push(if is_fleet_author(login) {
                format!(
                    "excluded_app_approval:{}",
                    app_approval_refusal(record, policy).unwrap_or_default()
                )
            } else {
                format!("excluded_human_approval:{login}")
            });
        }
    }
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
            && verdict_counts(review, Verdict::Approve, policy, author)
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
                && verdict_counts(comment, Verdict::Approve, policy, author)
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
    /// The approval deciding this head was posted by the agent now arming
    /// it (both stamped with the same [`AGENT_ENV`] id): an agent cannot
    /// approve its own landing.
    SelfApproved {
        /// The head.
        head: String,
        /// The agent that both approved and armed.
        agent: String,
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
                 on that commit, from a listed reviewer, or where none is listed from the \
                 fleet App (a `ghapp` verdict comment) or a human the base lists in \
                 `[auto_merge] human_reviewers`, never the PR author, with no later HOLD \
                 naming it. This repository arms only approved \
                 heads; left disarmed. Arm deliberately with `--arm`",
                short(head)
            ),
            Self::SelfApproved { head, agent } => format!(
                "the approval of head {} was posted by this same agent ({agent}); an agent \
                 cannot approve its own landing. Left disarmed until another agent approves \
                 it, or someone arms it with `--arm`",
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
        if !review
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
        verdict_counts(review, verdict, policy, author).then(|| (at(review), verdict))
    });
    let comment_events = comments.iter().filter_map(|comment| {
        let (verdict, marker_head) = marker_verdict(comment.get("body")?.as_str()?)?;
        if marker_head != head.to_ascii_lowercase()
            || !verdict_counts(comment, verdict, policy, author)
        {
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
    let gate = match find_approval_for_author(head, reviews, comments, policy, None, author) {
        Some(approval) => HeadGate::Approved { approval },
        None => HeadGate::NotApproved {
            head: head.to_owned(),
        },
    };
    refuse_self_approval(gate, head, comments)
}

/// `gate`, or [`HeadGate::SelfApproved`] when its approval is a marker
/// comment stamped with this process's agent id. Reviews, markers without an
/// agent stamp, and an arming process without an agent id pass unchanged; a
/// shared session id never refuses.
fn refuse_self_approval(gate: HeadGate, head: &str, comments: &[Value]) -> HeadGate {
    let HeadGate::Approved { approval } = &gate else {
        return gate;
    };
    let mut approval = approval;
    while let Approval::Carried {
        approval: inner, ..
    } = approval
    {
        approval = inner;
    }
    let Approval::Marker {
        comment_id: Some(id),
        ..
    } = approval
    else {
        return gate;
    };
    let Some(mine) = arming_agent() else {
        return gate;
    };
    let stamped = comments
        .iter()
        .find(|comment| comment.get("id").and_then(Value::as_u64) == Some(*id))
        .and_then(|comment| comment.get("body").and_then(Value::as_str))
        .and_then(stamped_agent);
    if stamped.as_deref() == Some(mine.as_str()) {
        return HeadGate::SelfApproved {
            head: head.to_owned(),
            agent: mine,
        };
    }
    gate
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
    host_default: bool,
) -> Result<ApprovalPolicy, String> {
    let raw = match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/{CONFIG_PATH}?ref={base}"),
    ]) {
        Ok(raw) => raw,
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => {
            if host_default && requires_review_by_default(repo) {
                let mut policy = ApprovalPolicy::without_config(repo, host_default);
                policy.signers = policy.signers.or_else(|| host_signers(repo));
                return Ok(policy);
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
    let mut policy = ApprovalPolicy::from_table_for_repo(&table, repo, host_default);
    policy.signers = policy.signers.or_else(|| host_signers(repo));
    policy.validate().map(|()| policy)
}

/// Read the policy from `base` on GitHub, treating a missing file as default.
pub fn read_policy(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<ApprovalPolicy, String> {
    read_policy_inner(run_gh, repo, base, false, host_review_default())
}

/// Read the policy as the repository-side rule, with no host in it: review
/// is on for [`REVIEW_REQUIRED_REPOS`] whatever this host's opt-out says
/// (`shipyard review-verdict`).
///
/// # Errors
///
/// When the config exists but cannot be read or parsed.
pub fn read_policy_repository_side(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
) -> Result<ApprovalPolicy, String> {
    let mut policy = read_policy_inner(run_gh, repo, base, false, true)?;
    // No host's signers file reaches the repository-side rule: only the base
    // config's list, and with none listed no App APPROVE counts.
    policy.signers = read_base_signers(run_gh, repo, base)?;
    policy.signer_source = SignerSource::Base;
    Ok(policy)
}

/// The base config's own `[auto_merge] reviewer_signers`, ignoring any host.
fn read_base_signers(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
) -> Result<Option<Vec<String>>, String> {
    match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/{CONFIG_PATH}?ref={base}"),
    ]) {
        Ok(raw) => Ok(ApprovalPolicy::from_table(
            &raw.parse::<toml::Table>()
                .map_err(|error| format!("{CONFIG_PATH} on {base} does not parse: {error}"))?,
        )
        .signers),
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => Ok(None),
        Err(detail) => Err(format!("{CONFIG_PATH} on {base} unreadable: {detail}")),
    }
}

/// Read the policy for the steward enqueue gate, where absence is unsafe.
pub fn read_policy_required(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
) -> Result<ApprovalPolicy, String> {
    read_policy_inner(run_gh, repo, base, true, host_review_default())
}

/// A review as the gate reads it.
const REVIEW_RECORD_JQ: &str = ".[] | {login: .user.login, type: .user.type, association: .author_association, at: .submitted_at, state: .state, commit_id: .commit_id, body: .body, performed_via_github_app_id: .performed_via_github_app.id}";
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
    Ok((
        refuse_self_approval(with_carry(run_gh, &carry, gate), head, &comments),
        events,
    ))
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
        let verdict = if review
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| state.eq_ignore_ascii_case("APPROVED"))
        {
            Verdict::Approve
        } else {
            Verdict::Hold
        };
        if verdict_counts(review, verdict, input.policy, input.author)
            && let Some(commit) = review.get("commit_id").and_then(Value::as_str)
        {
            named.push((at(review), commit.to_ascii_lowercase()));
        }
    }
    for comment in input.comments {
        if let Some((verdict, head)) = comment
            .get("body")
            .and_then(Value::as_str)
            .and_then(marker_verdict)
            && verdict_counts(comment, verdict, input.policy, input.author)
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
    let rebase = match merge_only_carry(run_gh, input.repo, input.pr, &from, input.head) {
        Ok(()) => false,
        Err(_) if input.policy.rebase_carry || host_rebase_carry() => {
            match rebase_carry(run_gh, input.repo, input.pr, &from, input.head) {
                Ok(record) => {
                    rebase::log_carry(input.repo, input.pr, &from, input.head, &record);
                    true
                }
                Err(_) => return gate,
            }
        }
        Err(_) => return gate,
    };
    HeadGate::Approved {
        approval: Approval::Carried {
            from_head: from,
            approval: Box::new(approval),
            rebase,
        },
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
///
/// It reads the whole blob, even for a public repository where the Python
/// guard reads only the first 8000 bytes from raw.githubusercontent.com: the
/// `gh` transport here returns text, which cannot carry a byte-exact ranged
/// body (issue #831). For a private repository both twins read the whole
/// blob, since gh sends no credentials to the raw host.
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
