//! The push claim fence: refuse a push to a pull-request branch while another
//! session holds a live `repair-claim:` on that pull request.
//!
//! This is coordination, not security. Every claim comment posts as the same
//! App, so an owner is only a name, and `git push --no-verify` skips the hook
//! that calls this. The fence therefore fails open, loudly, when GitHub cannot
//! be read within its deadline: refusing every repair push during a GitHub
//! incident would buy no protection. It is off unless `SHIPYARD_CLAIM_FENCE=on`
//! (default-off rollout). See `docs/claim-fence.md`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::work_claim::{HeldClaim, held_claims_in_records};

/// The switch: the fence runs only with `SHIPYARD_CLAIM_FENCE=on`.
pub(crate) const SWITCH_ENV: &str = "SHIPYARD_CLAIM_FENCE";
/// The claim owner this session wrote in its own `repair-claim:`.
pub(crate) const OWNER_ENV: &str = "SHIPYARD_CLAIM_OWNER";
/// Set by `shipyard pr` on its own push to the head it already checked, so
/// the repository's pre-push hook does not check the same push again.
pub(crate) const DONE_ENV: &str = "SHIPYARD_CLAIM_FENCE_DONE";
/// How long to wait before the one retry of a failed read.
pub(crate) const RETRY_DELAY: Duration = Duration::from_secs(2);
/// The whole check, every read and the retry included, ends by then.
pub(crate) const DEADLINE: Duration = Duration::from_secs(15);
/// A claim whose time is unreadable ends this long after it was posted.
pub(crate) const UNREADABLE_TIME_LIFETIME_HOURS: i64 = 4;

/// The same record shape `shipyard advance` reads comments in.
const COMMENT_RECORD_JQ: &str = ".[] | {id: .id, type: .user.type, association: .author_association, at: .created_at, body: .body}";

/// A GitHub read scoped to a repository: `(repo, gh argv, timeout)`.
pub(crate) type Read<'a> = &'a dyn Fn(&str, &[String], Duration) -> Result<String, String>;
/// A GitHub write scoped to a repository: `(repo, gh argv)`.
pub(crate) type Write<'a> = &'a dyn Fn(&str, &[String]) -> Result<String, String>;

/// Everything the fence reads from outside.
pub(crate) struct FenceIo<'a> {
    pub(crate) read: Read<'a>,
    pub(crate) sleep: &'a dyn Fn(Duration),
    /// Time spent since the check started.
    pub(crate) elapsed: &'a dyn Fn() -> Duration,
    pub(crate) now: DateTime<Utc>,
}

/// Who is pushing, and whether the fence is on.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct FenceEnv {
    /// `SHIPYARD_CLAIM_OWNER`, when set.
    pub(crate) owner: Option<String>,
    /// `SHIPYARD_CLAIM_FENCE=on`.
    pub(crate) enabled: bool,
}

impl FenceEnv {
    pub(crate) fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Self {
        let owner = env(OWNER_ENV)
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        let enabled = env(SWITCH_ENV).is_some_and(|value| value.trim().eq_ignore_ascii_case("on"));
        Self { owner, enabled }
    }
}

/// The fence's answer for one push.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Fence {
    /// No foreign live claim on any pushed branch's open pull request.
    Allow,
    /// The fence is off (the default).
    Disabled,
    /// A foreign live claim: the text says whose and how to proceed.
    Refuse(String),
    /// GitHub could not be read in time; the push goes ahead.
    FailOpen(String),
}

impl Fence {
    /// The line to print on stderr, if any.
    pub(crate) fn message(&self) -> Option<String> {
        match self {
            Self::Allow | Self::Disabled => None,
            Self::Refuse(text) => Some(text.clone()),
            Self::FailOpen(reason) => Some(format!(
                "claim-fence: could not read claims ({reason}); pushing without the check"
            )),
        }
    }
}

/// A check's answer and the warnings it raised on the way.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Outcome {
    pub(crate) fence: Fence,
    pub(crate) notes: Vec<String>,
}

/// Judge a push of `branches` to `repo`.
pub(crate) fn check_push(
    io: &FenceIo<'_>,
    env: &FenceEnv,
    repo: &str,
    branches: &[String],
) -> Outcome {
    let mut notes = Vec::new();
    if !env.enabled {
        return Outcome {
            fence: Fence::Disabled,
            notes,
        };
    }
    let fence = judge(io, env, repo, branches, &mut notes);
    Outcome { fence, notes }
}

fn judge(
    io: &FenceIo<'_>,
    env: &FenceEnv,
    repo: &str,
    branches: &[String],
    notes: &mut Vec<String>,
) -> Fence {
    let lifetime = chrono::Duration::hours(UNREADABLE_TIME_LIFETIME_HOURS);
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
            let held = held_claims_in_records(&comments);
            for claim in held
                .iter()
                .filter(|claim| crate::work_claim::claim_expiry(&claim.until).is_none())
            {
                notes.push(match claim.expires(Some(lifetime)) {
                    Some(end) => format!(
                        "claim-fence: comment {} claims for {} with an unreadable time `{}`; it counts until {} ({UNREADABLE_TIME_LIFETIME_HOURS}h after it was posted)",
                        claim.comment_id, claim.owner, claim.until, end.to_rfc3339()
                    ),
                    None => format!(
                        "claim-fence: comment {} claims for {} with an unreadable time `{}` and no readable post time; it counts as live",
                        claim.comment_id, claim.owner, claim.until
                    ),
                });
            }
            let foreign = held.iter().find(|claim| {
                claim.live_at(io.now, Some(lifetime))
                    && env.owner.as_deref() != Some(claim.owner.as_str())
            });
            if let Some(claim) = foreign {
                return Fence::Refuse(refusal(repo, branch, pr, claim));
            }
        }
    }
    Fence::Allow
}

/// No override is named here: a refusal points only at the take.
fn refusal(repo: &str, branch: &str, pr: u64, claim: &HeldClaim) -> String {
    format!(
        "claim-fence: refusing push to {repo} {branch}: pull request #{pr} has a live \
         repair claim by {owner} until {until} (comment {id}).\n\
         claim-fence: to take it over, run `shipyard claim take --repo {repo} --pr {pr} --from \
         {owner} --reason \"...\"`, naming the owner exactly as written (owner names can \
         collide), then push again.",
        owner = claim.owner,
        until = claim.until,
        id = claim.comment_id,
    )
}

/// Whether `shipyard pr` already checked this push: `done` (from
/// [`DONE_ENV`]) is a full SHA and every ref the hook was handed pushes it.
pub(crate) fn already_checked(stdin: &str, done: Option<&str>) -> bool {
    let Some(done) = done.map(str::trim) else {
        return false;
    };
    if done.len() != 40 || !done.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let shas: Vec<&str> = stdin
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split_whitespace().nth(1).unwrap_or(""))
        .collect();
    !shas.is_empty() && shas.iter().all(|sha| sha.eq_ignore_ascii_case(done))
}

/// One read, retried once after [`RETRY_DELAY`] when it fails, all inside
/// what is left of [`DEADLINE`].
fn read_retry(io: &FenceIo<'_>, repo: &str, args: &[String]) -> Result<String, String> {
    let remaining = || DEADLINE.saturating_sub((io.elapsed)());
    let out_of_time = || format!("the check passed its {}s deadline", DEADLINE.as_secs());
    if remaining().is_zero() {
        return Err(out_of_time());
    }
    let first = match (io.read)(repo, args, remaining()) {
        Ok(out) => return Ok(out),
        Err(error) => error,
    };
    if remaining() <= RETRY_DELAY {
        return Err(format!("{first} (no time left to retry)"));
    }
    (io.sleep)(RETRY_DELAY);
    if remaining().is_zero() {
        return Err(out_of_time());
    }
    (io.read)(repo, args, remaining()).map_err(|error| format!("{error} (after one retry)"))
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
    write: Write<'_>,
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
    let lifetime = chrono::Duration::hours(UNREADABLE_TIME_LIFETIME_HOURS);
    let live: Vec<HeldClaim> = held_claims_in_records(&comments)
        .into_iter()
        .filter(|claim| claim.live_at(io.now, Some(lifetime)))
        .collect();
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
