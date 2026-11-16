//! The push claim fence: refuse a push to a pull-request branch while another
//! session holds a live `repair-claim:` on that pull request.
//!
//! This is coordination, not security. Every claim comment posts as the same
//! App, so an owner is only a name, and `git push --no-verify` skips the hook
//! that calls this. The fence therefore fails open, loudly, when GitHub cannot
//! be read: refusing every repair push during a GitHub incident would buy no
//! protection. See `docs/claim-fence.md`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::work_claim::{LiveClaim, live_claims_in_records};

/// The kill switch: `SHIPYARD_CLAIM_FENCE=off` turns the fence off.
pub(crate) const SWITCH_ENV: &str = "SHIPYARD_CLAIM_FENCE";
/// The claim owner this session wrote in its own `repair-claim:`.
pub(crate) const OWNER_ENV: &str = "SHIPYARD_CLAIM_OWNER";
/// How long to wait before the one retry of a failed read.
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);

/// The same record shape `shipyard advance` reads comments in.
const COMMENT_RECORD_JQ: &str = ".[] | {id: .id, type: .user.type, association: .author_association, at: .created_at, body: .body}";

/// A GitHub call scoped to a repository: `(repo, gh argv)`.
pub(crate) type Gh<'a> = &'a dyn Fn(&str, &[String]) -> Result<String, String>;

/// Everything the fence reads from outside.
pub(crate) struct FenceIo<'a> {
    pub(crate) read: Gh<'a>,
    pub(crate) sleep: &'a dyn Fn(Duration),
    pub(crate) now: DateTime<Utc>,
}

/// Who is pushing, and whether the fence is on.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FenceEnv {
    /// `SHIPYARD_CLAIM_OWNER`, when set to a plain token.
    pub(crate) owner: Option<String>,
    /// `SHIPYARD_CLAIM_FENCE=off`.
    pub(crate) disabled: bool,
}

impl FenceEnv {
    pub(crate) fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let owner = env(OWNER_ENV)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let disabled =
            env(SWITCH_ENV).is_some_and(|value| value.trim().eq_ignore_ascii_case("off"));
        Self { owner, disabled }
    }
}

/// The fence's answer for one push.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Fence {
    /// No foreign live claim on any pushed branch's open pull request.
    Allow,
    /// The kill switch is on.
    Disabled,
    /// A foreign live claim: the text says whose and how to proceed.
    Refuse(String),
    /// GitHub could not be read after one retry; the push goes ahead.
    FailOpen(String),
}

impl Fence {
    /// The line to print on stderr, if any.
    pub(crate) fn message(&self) -> Option<String> {
        match self {
            Self::Allow => None,
            Self::Disabled => Some(format!(
                "claim-fence: {SWITCH_ENV}=off; pushing without the check"
            )),
            Self::Refuse(text) => Some(text.clone()),
            Self::FailOpen(reason) => Some(format!(
                "claim-fence: could not read claims ({reason}); pushing without the check"
            )),
        }
    }
}

/// Judge a push of `branches` to `repo`.
pub(crate) fn check_push(
    io: &FenceIo<'_>,
    env: &FenceEnv,
    repo: &str,
    branches: &[String],
) -> Fence {
    if env.disabled {
        return Fence::Disabled;
    }
    for branch in branches {
        let prs = match open_prs(io, repo, branch) {
            Ok(prs) => prs,
            Err(reason) => return Fence::FailOpen(reason),
        };
        for pr in prs {
            let comments = match read_comments(io, repo, pr) {
                Ok(comments) => comments,
                Err(reason) => return Fence::FailOpen(reason),
            };
            let foreign: Vec<LiveClaim> = live_claims_in_records(&comments, io.now)
                .into_iter()
                .filter(|claim| env.owner.as_deref() != Some(claim.owner.as_str()))
                .collect();
            if let Some(claim) = foreign.first() {
                return Fence::Refuse(refusal(repo, branch, pr, claim, env));
            }
        }
    }
    Fence::Allow
}

fn refusal(repo: &str, branch: &str, pr: u64, claim: &LiveClaim, env: &FenceEnv) -> String {
    let you = env.owner.as_deref().map_or_else(
        || format!("{OWNER_ENV} is unset"),
        |owner| format!("you are {owner}"),
    );
    format!(
        "claim-fence: refusing push to {repo} {branch}: pull request #{pr} has a live \
         repair claim by {owner} until {until} (comment {id}); {you}.\n\
         claim-fence: set {OWNER_ENV}={owner} if this is your claim, or take it over with \
         `shipyard claim take --repo {repo} --pr {pr} --from {owner} --reason \"...\"`, then push again.",
        owner = claim.owner,
        until = claim.until,
        id = claim.comment_id,
    )
}

/// One read, retried once after [`RETRY_DELAY`] when it fails.
fn read_retry(io: &FenceIo<'_>, repo: &str, args: &[String]) -> Result<String, String> {
    if let Ok(out) = (io.read)(repo, args) {
        return Ok(out);
    }
    (io.sleep)(RETRY_DELAY);
    (io.read)(repo, args).map_err(|error| format!("{error} (after one retry)"))
}

pub(crate) fn open_prs(io: &FenceIo<'_>, repo: &str, branch: &str) -> Result<Vec<u64>, String> {
    let owner = repo.split('/').next().unwrap_or_default();
    let path = format!(
        "repos/{repo}/pulls?state=open&head={}",
        query_escape(&format!("{owner}:{branch}"))
    );
    let raw = read_retry(
        io,
        repo,
        &[
            "api".to_owned(),
            path,
            "--jq".to_owned(),
            ".[].number".to_owned(),
        ],
    )?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            line.trim()
                .parse::<u64>()
                .map_err(|_| format!("unexpected pull request number {line:?}"))
        })
        .collect()
}

fn read_comments(io: &FenceIo<'_>, repo: &str, pr: u64) -> Result<Vec<Value>, String> {
    let raw = read_retry(
        io,
        repo,
        &[
            "api".to_owned(),
            "--paginate".to_owned(),
            format!("repos/{repo}/issues/{pr}/comments"),
            "--jq".to_owned(),
            COMMENT_RECORD_JQ.to_owned(),
        ],
    )?;
    raw.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).map_err(|error| format!("malformed comment record: {error}"))
        })
        .collect()
}

/// Percent-encode a query value, keeping what GitHub's `head` filter reads
/// plainly (`owner:branch/with/slashes`).
fn query_escape(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~:/".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

/// The branches a pre-push hook's stdin names
/// (`<local ref> <local sha> <remote ref> <remote sha>` per line).
pub(crate) fn pushed_branches(stdin: &str) -> Vec<String> {
    let mut branches: Vec<String> = stdin
        .lines()
        .filter_map(|line| line.split_whitespace().nth(2))
        .filter_map(|remote_ref| remote_ref.strip_prefix("refs/heads/"))
        .map(str::to_owned)
        .collect();
    branches.sort();
    branches.dedup();
    branches
}

/// Why a take was not posted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TakeError(pub(crate) String);

/// Take `from`'s live claim on `repo#pr`: post
/// `repair-claim-taken: <owner> from <from> reason=<reason>` through `write`
/// and read the posted comment back from the response.
pub(crate) fn take(
    io: &FenceIo<'_>,
    write: Gh<'_>,
    env: &FenceEnv,
    repo: &str,
    pr: u64,
    from: &str,
    reason: &str,
) -> Result<String, TakeError> {
    let owner = env.owner.as_deref().ok_or_else(|| {
        TakeError(format!(
            "{OWNER_ENV} is unset: set it to the name the claim should pass to"
        ))
    })?;
    for (name, value) in [(OWNER_ENV, owner), ("--from", from)] {
        if !crate::owner_registry::safe_value(value) || value.len() > 64 {
            return Err(TakeError(format!(
                "{name} must be one plain token (letters, digits, ._:/@+-), at most 64 bytes"
            )));
        }
    }
    if owner == from {
        return Err(TakeError(format!("{from} is already you")));
    }
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    if reason.is_empty() {
        return Err(TakeError(
            "--reason must say why the claim is taken".to_owned(),
        ));
    }
    if reason.len() > 300 {
        return Err(TakeError("--reason is longer than 300 bytes".to_owned()));
    }
    let comments = read_comments(io, repo, pr).map_err(TakeError)?;
    let live = live_claims_in_records(&comments, io.now);
    if !live.iter().any(|claim| claim.owner == from) {
        let held = if live.is_empty() {
            "none".to_owned()
        } else {
            live.iter()
                .map(|claim| claim.owner.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(TakeError(format!(
            "{from} holds no live claim on {repo}#{pr} (live: {held}); nothing to take"
        )));
    }
    let take_line = format!("repair-claim-taken: {owner} from {from} reason={reason}");
    let response = write(
        repo,
        &[
            "api".to_owned(),
            format!("repos/{repo}/issues/{pr}/comments"),
            "--raw-field".to_owned(),
            format!("body={take_line}"),
        ],
    )
    .map_err(|error| TakeError(format!("posting the take failed: {error}")))?;
    let posted: Value = serde_json::from_str(response.trim())
        .map_err(|error| TakeError(format!("the take's response was unreadable: {error}")))?;
    let body = posted.get("body").and_then(Value::as_str).unwrap_or("");
    if !body
        .lines()
        .any(|posted_line| posted_line.trim() == take_line)
    {
        return Err(TakeError(
            "the posted comment does not carry the take line; check the pull request".to_owned(),
        ));
    }
    let url = posted
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or("(no url)");
    Ok(format!("{take_line}\nposted: {url}"))
}

#[cfg(test)]
mod tests;
