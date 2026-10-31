//! The `fix` every rules event carries: enough for an agent to act without
//! investigating.
//!
//! Each fix names the exact subject (`what`), where to look (`evidence`, a URL
//! or a read-only command), the cause class (`cause`), the next command or
//! edit (`command`), whether an agent may apply it unattended
//! (`safe_to_auto_apply`), and how to confirm it worked (`verify`). Commands
//! are written for a checkout of the repository with `ghapp` on `PATH`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::super::{CheckFact, PrHistory};
use super::{Alarm, TrailEntry};

/// What to do about one event.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fix {
    /// The exact subject: pull request, head, check, test, host.
    pub what: String,
    /// Where the evidence is: a URL or a read-only command.
    pub evidence: String,
    /// Cause class.
    pub cause: String,
    /// The next command or edit.
    pub command: String,
    /// Whether an agent may apply `command` unattended.
    pub safe_to_auto_apply: bool,
    /// How to confirm it worked.
    pub verify: String,
}

/// The pull request's web URL.
#[must_use]
pub fn pr_url(repo: &str, pr: &PrHistory) -> String {
    if pr.url.is_empty() {
        format!("https://github.com/{repo}/pull/{}", pr.number)
    } else {
        pr.url.clone()
    }
}

/// A trail entry's web URL.
#[must_use]
pub fn entry_url(repo: &str, pr: &PrHistory, entry: &TrailEntry) -> String {
    let anchor = if entry.review {
        "pullrequestreview"
    } else {
        "issuecomment"
    };
    format!("{}#{anchor}-{}", pr_url(repo, pr), entry.id)
}

fn marker(sha: &str) -> String {
    format!(
        "`verdict: APPROVE head:{sha}` or `verdict: HOLD head:{sha}` as the comment's last line"
    )
}

fn subject(repo: &str, pr: &PrHistory, sha: &str) -> String {
    format!("{repo}#{} head {sha}", pr.number)
}

/// A READY head no verdict names: route it to the repository's reviewer.
#[must_use]
pub fn ready(
    repo: &str,
    pr: &PrHistory,
    sha: &str,
    ready: &TrailEntry,
    reviewer: Option<&str>,
) -> Fix {
    let url = pr_url(repo, pr);
    let to = reviewer.map_or_else(|| "the repository's reviewer".to_owned(), str::to_owned);
    Fix {
        what: subject(repo, pr, sha),
        evidence: entry_url(repo, pr, ready),
        cause: "unreviewed_head".to_owned(),
        command: format!(
            "route head {sha} of {url} to {to}; the reviewer posts {}",
            marker(sha)
        ),
        safe_to_auto_apply: reviewer.is_some(),
        verify: format!("a comment on {url} whose last line names head {sha}"),
    }
}

/// A head with neither a READY nor a verdict.
#[must_use]
pub fn head_unreviewed(repo: &str, pr: &PrHistory, sha: &str) -> Fix {
    let url = pr_url(repo, pr);
    Fix {
        what: subject(repo, pr, sha),
        evidence: url.clone(),
        cause: "no_ready".to_owned(),
        command: format!(
            "ask the author of {url} to post `READY head:{sha}` on the pull request once \
             the head is ready, then route it to a reviewer"
        ),
        safe_to_auto_apply: false,
        verify: format!("a READY comment naming {sha} on {url}"),
    }
}

/// A posted verdict: relay it to the author.
#[must_use]
pub fn verdict(repo: &str, pr: &PrHistory, entry: &TrailEntry, approve: bool, sha: &str) -> Fix {
    let url = entry_url(repo, pr, entry);
    let author = pr.author.as_deref().unwrap_or("the author");
    let next = if approve {
        format!("it may arm head {sha} bound to it once its checks pass")
    } else {
        "it fixes what the HOLD names and posts a new READY".to_owned()
    };
    Fix {
        what: subject(repo, pr, sha),
        evidence: url.clone(),
        cause: if approve { "approved" } else { "held" }.to_owned(),
        command: format!(
            "relay {url} to the session of {author} on {repo}#{}: {next}",
            pr.number
        ),
        safe_to_auto_apply: true,
        verify: if approve {
            format!("{repo}#{} is armed at {sha}, or merged", pr.number)
        } else {
            format!("a new head on {repo}#{} after the HOLD", pr.number)
        },
    }
}

/// Armed or queued with no approval of the head.
#[must_use]
pub fn armed_unapproved(repo: &str, pr: &PrHistory, sha: &str, reviewer: Option<&str>) -> Fix {
    let url = pr_url(repo, pr);
    let to = reviewer.map_or_else(|| "the repository's reviewer".to_owned(), str::to_owned);
    Fix {
        what: subject(repo, pr, sha),
        evidence: url.clone(),
        cause: "unreviewed_arm".to_owned(),
        command: format!(
            "route head {sha} of {url} to {to} for a verdict ({}); if it must not land \
             first, disarm: ghapp pr merge {} --repo {repo} --disable-auto",
            marker(sha),
            pr.number
        ),
        safe_to_auto_apply: false,
        verify: format!(
            "a comment on {url} whose last line approves {sha}, or the pull request disarmed"
        ),
    }
}

/// A red or cancelled required check on an armed or approved head.
#[must_use]
pub fn required_red(repo: &str, pr: &PrHistory, sha: &str, check: &CheckFact) -> Fix {
    let conclusion = check.conclusion.as_deref().unwrap_or_default();
    let tests: Vec<&str> = check.signatures.iter().map(String::as_str).collect();
    let cause = if conclusion == "cancelled" {
        "cancelled"
    } else if tests.iter().any(|test| !test.starts_with("error: ")) {
        "test_failure"
    } else if tests.is_empty() {
        "unread_failure"
    } else {
        "step_error"
    };
    let log = format!(
        "ghapp run view --repo {repo} --job {} --log-failed",
        check.id
    );
    let command = if conclusion == "cancelled" {
        format!(
            "{log}  # find why it was cancelled; if nothing implicates the head, rerun it: \
             ghapp run rerun --repo {repo} --job {}",
            check.id
        )
    } else if tests.is_empty() {
        format!("{log}  # the log names the failing step; fix it on the head and push")
    } else {
        format!(
            "fix {} on {repo}#{} and push a new head ({log} shows the failure)",
            tests.join(", "),
            pr.number
        )
    };
    Fix {
        what: format!(
            "{} on {}{}",
            check.name,
            subject(repo, pr, sha),
            if tests.is_empty() {
                String::new()
            } else {
                format!(": {}", tests.join(", "))
            }
        ),
        evidence: format!("https://github.com/{repo}/runs/{}", check.id),
        cause: cause.to_owned(),
        command,
        safe_to_auto_apply: false,
        verify: format!(
            "{} passes on the new head of {repo}#{}",
            check.name, pr.number
        ),
    }
}

/// A failed merge group named for the pull request.
#[must_use]
pub fn group_red(repo: &str, pr: &PrHistory, run_id: u64, tests: &[String]) -> Fix {
    Fix {
        what: format!(
            "merge group run {run_id} for {repo}#{}{}",
            pr.number,
            if tests.is_empty() {
                String::new()
            } else {
                format!(": {}", tests.join(", "))
            }
        ),
        evidence: format!("https://github.com/{repo}/actions/runs/{run_id}"),
        cause: if tests.is_empty() {
            "unread_failure"
        } else {
            "test_failure"
        }
        .to_owned(),
        command: format!(
            "ghapp run view {run_id} --repo {repo} --log-failed  # a group is named for its last \
             entry: confirm the failure is this pull request's before pushing a fix"
        ),
        safe_to_auto_apply: false,
        verify: format!("{repo}#{} merges, or its next group passes", pr.number),
    }
}

/// A queue removal for a reason other than a merge.
#[must_use]
pub fn queue_removed(repo: &str, pr: &PrHistory, reason: &str) -> Fix {
    Fix {
        what: format!("{repo}#{} removed from the merge queue", pr.number),
        evidence: pr_url(repo, pr),
        cause: reason.to_owned(),
        command: format!(
            "shipyard landing --pr {}  # names the removal and the way forward; a same-head \
             re-enqueue after failed_checks needs a new head",
            pr.number
        ),
        safe_to_auto_apply: false,
        verify: format!(
            "{repo}#{} back in the queue at a new head, or merged",
            pr.number
        ),
    }
}

/// A queue entry waiting too long.
#[must_use]
pub fn queue_stuck(repo: &str, base: &str, pr: &PrHistory, since: &str) -> Fix {
    Fix {
        what: format!("{repo}#{} queued since {since}", pr.number),
        evidence: format!("https://github.com/{repo}/queue/{base}"),
        cause: "queue_stuck".to_owned(),
        command: format!(
            "shipyard landing --pr {}  # shows its group's run; a run waiting on a runner is a \
             capacity problem, not this pull request's",
            pr.number
        ),
        safe_to_auto_apply: false,
        verify: format!("{repo}#{} merged or removed", pr.number),
    }
}

/// An approved head nobody armed.
#[must_use]
pub fn approved_unarmed(repo: &str, pr: &PrHistory, sha: &str, approval: &TrailEntry) -> Fix {
    Fix {
        what: subject(repo, pr, sha),
        evidence: entry_url(repo, pr, approval),
        cause: "approved_unarmed".to_owned(),
        command: format!(
            "ghapp pr merge {} --repo {repo} --auto --merge --match-head-commit {sha}",
            pr.number
        ),
        safe_to_auto_apply: true,
        verify: format!("{repo}#{} armed or queued at {sha}", pr.number),
    }
}

/// An approved head GitHub reports `DIRTY` or `BEHIND`.
#[must_use]
pub fn approved_blocked(repo: &str, pr: &PrHistory, sha: &str, merge_state: &str) -> Fix {
    let command = if merge_state == "BEHIND" {
        format!(
            "ghapp pr update-branch {} --repo {repo}  # a merge-only update keeps the approval \
             when the pull request's own lines are unchanged",
            pr.number
        )
    } else {
        format!(
            "merge the base into {repo}#{} and resolve the conflict; the new head needs a new \
             verdict",
            pr.number
        )
    };
    Fix {
        what: subject(repo, pr, sha),
        evidence: pr_url(repo, pr),
        cause: merge_state.to_ascii_lowercase(),
        command,
        safe_to_auto_apply: false,
        verify: format!("{repo}#{} is CLEAN or BLOCKED only on checks", pr.number),
    }
}

/// A host alarm issue.
#[must_use]
pub fn host_alarm(alarm: &Alarm) -> Fix {
    let url = format!("https://github.com/{}/issues/{}", alarm.repo, alarm.number);
    Fix {
        what: alarm.title.clone(),
        evidence: url.clone(),
        cause: "host_alarm".to_owned(),
        command: format!("follow the runbook in {url}; it names the host and the next step"),
        safe_to_auto_apply: false,
        verify: format!("{url} closed by its recovery check"),
    }
}

/// An input event's own `fix`, when it carries a complete one.
#[must_use]
pub fn from_input(kind: &str, key: &str, rest: &BTreeMap<String, Value>) -> Fix {
    if let Some(fix) = rest
        .get("fix")
        .and_then(|fix| serde_json::from_value::<Fix>(fix.clone()).ok())
        .filter(|fix| {
            [
                &fix.what,
                &fix.evidence,
                &fix.cause,
                &fix.command,
                &fix.verify,
            ]
            .iter()
            .all(|field| !field.is_empty())
        })
    {
        return fix;
    }
    Fix {
        what: format!("{kind} {key}"),
        evidence: "the reader that wrote this input line".to_owned(),
        cause: "input_without_fix".to_owned(),
        command: format!(
            "make the reader that writes `{kind}` lines set a complete `fix` (what, evidence, \
             cause, command, safe_to_auto_apply, verify)"
        ),
        safe_to_auto_apply: false,
        verify: format!("the next `{kind}` line carries a complete fix"),
    }
}

/// A HOLD on the current head that no push has answered.
#[must_use]
pub fn held_unrepaired(repo: &str, pr: &PrHistory, sha: &str, hold: &TrailEntry) -> Fix {
    Fix {
        what: subject(repo, pr, sha),
        evidence: entry_url(repo, pr, hold),
        cause: "held_unrepaired".to_owned(),
        command: format!(
            "read the HOLD, push a repair to {} (or label the pull request owner-decision, \
             superseded or PARKED/<why> if it is not the author's to repair)",
            pr.head_ref
        ),
        safe_to_auto_apply: false,
        verify: format!(
            "{repo}#{} has a new head and a verdict naming it",
            pr.number
        ),
    }
}

/// A pull request set aside by a label: not a stall.
#[must_use]
pub fn labelled(repo: &str, pr: &PrHistory, sha: &str, state: &str, label: &str) -> Fix {
    Fix {
        what: subject(repo, pr, sha),
        evidence: pr_url(repo, pr),
        cause: state.to_owned(),
        command: format!("none; remove the {label} label when it should be worked again"),
        safe_to_auto_apply: false,
        verify: format!("{repo}#{} no longer carries {label}", pr.number),
    }
}
