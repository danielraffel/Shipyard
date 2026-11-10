//! The one thing to do next about a pull request, from its `blocked_on`
//! token.
//!
//! `blocked_on` names why a pull request is not merging; this names who moves
//! it and what that party does, in a form an agent can follow without reading
//! GitHub itself. It is derived from the same row the admission pass judged,
//! so it cannot say something admission would refuse. Pure: no reads.
//!
//! `actor` is who acts. `author` means the caller (the agent that owns the
//! branch) acts now; `owner` means the repository owner decides and the
//! caller stops; every other actor means the caller has nothing to do and
//! waits. `stop` is true when the caller may end its session: the owner must
//! act, or the pull request is merged or closed.

use serde::Serialize;

use super::blocked_on::Stall;
use crate::merge_steward::{
    OWNER_DECISION_LABEL, PARKED_LABEL_PREFIX, SUPERSEDED_LABEL, StewardPolicy, StewardPullRequest,
};

/// Who moves the pull request next.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Actor {
    /// The branch's author (the calling agent) acts now.
    Author,
    /// A reviewer posts a verdict.
    Reviewer,
    /// CI is running.
    Ci,
    /// The merge steward or the merge queue acts on its own.
    Steward,
    /// The repository owner decides; the caller stops.
    Owner,
    /// The fleet operator; the caller waits for the next pass.
    Operator,
    /// Nobody: the pull request is terminal.
    Nobody,
}

/// What to do next, and who does it.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct NextAction {
    /// Who acts.
    pub actor: Actor,
    /// The action, imperative, with the command to run when one exists.
    pub action: String,
    /// The caller may stop: the owner must act, or the PR is terminal.
    pub stop: bool,
}

impl NextAction {
    /// Whether the caller has something to do (act, or stop).
    #[must_use]
    pub const fn actionable(&self) -> bool {
        matches!(self.actor, Actor::Author | Actor::Owner) || self.stop
    }

    fn new(actor: Actor, action: String) -> Self {
        Self {
            actor,
            action,
            stop: false,
        }
    }

    fn stop(actor: Actor, action: String) -> Self {
        Self {
            actor,
            action,
            stop: true,
        }
    }

    /// A merged pull request.
    #[must_use]
    pub fn merged(head_branch: &str, base: &str, merged_at: Option<&str>) -> Self {
        let at = merged_at.map_or_else(String::new, |at| format!(" at {at}"));
        Self::stop(
            Actor::Nobody,
            format!(
                "Merged{at}. Nothing to do here. A pull request stacked on `{head_branch}` must \
                 be retargeted to `{base}` (then merge `{base}` into it) before the steward \
                 admits it."
            ),
        )
    }

    /// A closed, unmerged pull request.
    #[must_use]
    pub fn closed() -> Self {
        Self::stop(
            Actor::Nobody,
            "Closed without merging. Reopen it deliberately, or stop.".to_owned(),
        )
    }

    /// A pull request whose base is not the branch the steward admits to.
    #[must_use]
    pub fn stacked(base: &str, default_branch: &str, open_parent: Option<u64>) -> Self {
        match open_parent {
            Some(parent) => Self::new(
                Actor::Steward,
                format!(
                    "Based on `{base}`, not `{default_branch}`: the steward admits only pull \
                     requests on `{default_branch}`. Nothing to do until #{parent} merges \
                     (`shipyard wait pr {parent} --state merged`); GitHub retargets this one \
                     when `{base}` is deleted, otherwise retarget it to `{default_branch}` then."
                ),
            ),
            None => Self::new(
                Actor::Author,
                format!(
                    "Based on `{base}`, not `{default_branch}`, and no open pull request targets \
                     `{base}`: retarget this pull request to `{default_branch}` now and merge \
                     `{default_branch}` into it (no rebase)."
                ),
            ),
        }
    }

    /// A pull request whose diff against its base is empty.
    #[must_use]
    pub fn empty_diff(base: &str) -> Self {
        Self::new(
            Actor::Author,
            format!(
                "The diff against `{base}` is empty (0 changed files): the change was dropped \
                 by a merge or rebase. Restore it on this branch and push once; a green run on \
                 nothing proves nothing."
            ),
        )
    }

    /// A row the pass could not match to an observed pull request.
    #[must_use]
    pub fn unclassified() -> Self {
        Self::new(
            Actor::Operator,
            "The pass could not classify this pull request. Nothing to do; next pass.".to_owned(),
        )
    }

    /// A row the pass deferred before evaluating it (see `pass_budget`).
    #[must_use]
    pub fn deferred_pass_budget() -> Self {
        Self::new(
            Actor::Steward,
            "The pass stopped before evaluating this pull request; the next pass takes it \
             first. Nothing to do."
                .to_owned(),
        )
    }
}

/// What the row's next action is derived from.
pub(super) struct RowContext<'a> {
    /// `OWNER/REPO`.
    pub repo: &'a str,
    /// The base branch the steward admits to.
    pub base: &'a str,
    /// The pull request as observed.
    pub pr: &'a StewardPullRequest,
    /// The repository's admission policy.
    pub policy: &'a StewardPolicy,
    /// The row's error, when the pass recorded one.
    pub error: Option<&'a str>,
}

/// The strings every token's text is built from.
struct Facts<'a> {
    token: &'a str,
    /// The token's `:`-suffixed argument (contexts, a label, a cause).
    arg: &'a str,
    detail: Option<&'a str>,
    /// `: <detail>` when there is one.
    detail_suffix: String,
    repo: &'a str,
    base: &'a str,
    number: u64,
    head: &'a str,
    /// The required contexts, comma-joined.
    required: String,
    error: Option<&'a str>,
    /// The policy's approval context (`review-verdict`).
    approval_context: &'a str,
}

/// The next action for one classified row.
#[must_use]
pub(super) fn for_row(context: &RowContext<'_>, stall: &Stall) -> NextAction {
    let token = stall.blocked_on.as_str();
    let (kind, arg) = token
        .split_once(':')
        .map_or((token, ""), |(kind, arg)| (kind, arg));
    let detail = stall.blocked_detail.as_deref();
    let facts = Facts {
        token,
        arg,
        detail,
        detail_suffix: detail.map_or_else(String::new, |detail| format!(": {detail}")),
        repo: context.repo,
        base: context.base,
        number: context.pr.number,
        head: context.pr.head_sha.as_str(),
        required: context
            .policy
            .required_checks
            .iter()
            .map(|check| check.context.as_str())
            .collect::<Vec<_>>()
            .join(","),
        error: context.error,
        approval_context: context.policy.approval_context.as_str(),
    };
    let mut next = callers_move(kind, &facts, stall)
        .or_else(|| code_owners_move(kind, &facts))
        .or_else(|| someone_elses_move(kind, &facts))
        .unwrap_or_else(|| {
            NextAction::new(
                Actor::Operator,
                format!(
                    "Unrecognized token {token}; see docs/automated-draining.md. Nothing to do."
                ),
            )
        });
    if context.policy.merge_queue
        && context.pr.merge_state.eq_ignore_ascii_case("BEHIND")
        && !matches!(next.actor, Actor::Author | Actor::Owner | Actor::Nobody)
    {
        let base = context.base;
        next.action = format!(
            "{} It is BEHIND {base}; do not merge {base}: the merge queue brings it up to date.",
            next.action
        );
    }
    next
}

/// The tokens the caller moves: it acts (`author`) or stops (`owner`).
fn callers_move(kind: &str, facts: &Facts<'_>, stall: &Stall) -> Option<NextAction> {
    let Facts {
        arg,
        detail_suffix,
        repo,
        base,
        number,
        head,
        ..
    } = facts;
    Some(match kind {
        "draft" => NextAction::new(
            Actor::Author,
            "Draft: mark it ready for review when it is.".to_owned(),
        ),
        "owner_decision" => NextAction::stop(
            Actor::Owner,
            format!(
                "Labelled {OWNER_DECISION_LABEL}: the owner decides. Stop and report; do not \
                 push."
            ),
        ),
        "excluded_label" => excluded_label(arg),
        "provenance_blocked" => NextAction::stop(
            Actor::Owner,
            format!("Labelled {arg}: provenance unresolved; the owner clears it. Stop and report."),
        ),
        "unmanaged" => NextAction::new(
            Actor::Author,
            format!(
                "The steward does not own this pull request: hand off its head with `shipyard \
                 runner steward-handoff --repo {repo} --pr {number} --head {head} --apply`, or \
                 open it with `shipyard pr`."
            ),
        ),
        "handoff_missing" => NextAction::new(
            Actor::Author,
            format!(
                "Labelled managed, but this head has no handoff receipt: `shipyard runner \
                 steward-handoff --repo {repo} --pr {number} --head {head} --apply`."
            ),
        ),
        "merge_state" => merge_state(arg, base, *number),
        "required_never_ran" => NextAction::new(
            Actor::Author,
            format!(
                "Required check(s) {arg} never ran on this head: merge {base} once (no rebase) \
                 and push once so CI starts, or dispatch the workflow on this branch."
            ),
        ),
        "required_failed" => NextAction::new(
            Actor::Author,
            format!(
                "Required check(s) {arg} failed on this head: `shipyard diagnose --pr {number} \
                 --repo {repo}` names the failing test; fix, push once. Checks that are not \
                 required do not gate."
            ),
        ),
        "held" => held(stall, repo, base, *number),
        "ejection_cap" => NextAction::new(
            Actor::Author,
            format!(
                "The merge queue removed this head too often since its last approval: read \
                 `shipyard landing --pr {number} --repo {repo}`, fix the cause, push once."
            ),
        ),
        "ejected_same_head" => ejected_same_head(facts),
        "carries_unapproved_commits" => NextAction::new(
            Actor::Author,
            format!(
                "This head carries commits of open {arg}, whose head is not \
                 approved{detail_suffix}: wait for {arg}'s approval, or drop its commits from \
                 this branch."
            ),
        ),
        "direct_merge_refused" => NextAction::stop(
            Actor::Owner,
            "No automatic admission on this base: a maintainer merges by hand. Stop and report."
                .to_owned(),
        ),
        "empty_diff" => NextAction::empty_diff(base),
        _ => return None,
    })
}

/// The tokens a code owner moves: the caller stops and reports.
fn code_owners_move(kind: &str, facts: &Facts<'_>) -> Option<NextAction> {
    let Facts {
        arg, detail_suffix, ..
    } = facts;
    Some(match kind {
        "waiting_code_owner" => NextAction::stop(
            Actor::Owner,
            format!(
                "Green and approved; the base requires a code-owner review and no code owner \
                 ({}) has approved this exact head. The steward notified them once and admits \
                 the head on the pass after one approves. Stop and report; do not push, do not \
                 label it {OWNER_DECISION_LABEL}.",
                arg.replace('+', ", ")
            ),
        ),
        "code_owner_review" => NextAction::stop(
            Actor::Owner,
            format!(
                "GitHub refused to admit this head pending a code-owner review{detail_suffix}. \
                 A code owner approves this exact head; the steward retries on its next pass. \
                 Stop and report; do not push."
            ),
        ),
        _ => return None,
    })
}

/// The tokens someone else moves: the caller waits.
fn someone_elses_move(kind: &str, facts: &Facts<'_>) -> Option<NextAction> {
    let Facts {
        token,
        arg,
        detail,
        detail_suffix,
        base,
        required,
        ..
    } = facts;
    let detail = detail.unwrap_or("");
    Some(match kind {
        "admitted" => NextAction::new(
            Actor::Nobody,
            "Admitted this pass: enqueued or merged. Nothing to do.".to_owned(),
        ),
        "queued" => NextAction::new(
            Actor::Steward,
            format!(
                "In the merge queue{}. Nothing to do: do not push, do not merge {base}.",
                facts
                    .detail
                    .map_or_else(String::new, |detail| format!(" ({detail})"))
            ),
        ),
        "ready" => NextAction::new(
            Actor::Steward,
            format!(
                "Approved and green: the steward admits it on its next pass. Nothing to do: do \
                 not push, do not arm, do not merge {base}."
            ),
        ),
        "required_pending" if awaiting_reviewer(facts) => awaiting_reviewer_action(),
        "required_pending" => NextAction::new(
            Actor::Ci,
            format!(
                "Required check(s) {detail} are running. Nothing to do; do not push (a push \
                 restarts them). Checks that are not required do not gate."
            ),
        ),
        "transient_rerun" => NextAction::new(
            Actor::Steward,
            format!(
                "The steward is rerunning an interrupted required check (run {detail}). Nothing \
                 to do."
            ),
        ),
        "no_verdict" => NextAction::new(
            Actor::Reviewer,
            format!(
                "Awaiting the standing reviewer; nothing for the author to do. Green on every \
                 required check ({required}); no reviewer verdict names this head. Do not push, \
                 do not merge {base}; the reviewer posts APPROVE or HOLD."
            ),
        ),
        "approval_excluded" => NextAction::new(
            Actor::Reviewer,
            format!(
                "An APPROVE names this head but does not count ({arg}{detail_suffix}). Nothing \
                 to do: a listed reviewer must post it."
            ),
        ),
        "waiting_full_test_tier" | "waiting_enqueue_requirements" => NextAction::new(
            Actor::Ci,
            format!("Admission waits on {token}. Nothing to do."),
        ),
        "generated_path_batch" => NextAction::new(
            Actor::Steward,
            format!(
                "Waits for queued {arg} to leave the merge queue: both change a generated \
                 path{detail_suffix}. Nothing to do; the steward admits it on the next pass after \
                 that. Do not push or merge {base} for this."
            ),
        ),
        "revalidating" | "admission_pending" => NextAction::new(
            Actor::Steward,
            format!(
                "The steward's last pass ended in {token}{detail_suffix}. Nothing to do; next \
                 pass."
            ),
        ),
        _ => return operators_move(kind, facts),
    })
}

/// Whether a `required_pending` row waits only on the approval context (the
/// `review-verdict` status is `pending` while the head awaits its reviewer).
fn awaiting_reviewer(facts: &Facts<'_>) -> bool {
    let approval = facts.approval_context.trim();
    !approval.is_empty()
        && facts.detail.is_some_and(|detail| {
            detail
                .split(',')
                .all(|name| name.trim().eq_ignore_ascii_case(approval))
        })
}

fn awaiting_reviewer_action() -> NextAction {
    NextAction::new(
        Actor::Reviewer,
        "Awaiting the standing reviewer; nothing for the author to do. The approval status is \
         pending, not failed: do not push, do not merge the base; the reviewer posts APPROVE or \
         HOLD."
            .to_owned(),
    )
}

/// The tokens only the fleet operator or the next pass resolves.
fn operators_move(kind: &str, facts: &Facts<'_>) -> Option<NextAction> {
    let Facts {
        token,
        detail_suffix,
        error,
        ..
    } = facts;
    Some(match kind {
        "invalid_head" => NextAction::new(
            Actor::Operator,
            "The head SHA is malformed. Nothing to do until a pass reads a full SHA.".to_owned(),
        ),
        "carried_commits_over_cap" | "carried_commits_unreadable" => NextAction::new(
            Actor::Operator,
            format!(
                "Whether this head carries unapproved commits could not be proven ({token}). \
                 Nothing to do; next pass."
            ),
        ),
        "approval_unread" => NextAction::new(
            Actor::Operator,
            format!(
                "The approval gate could not be read this pass{}. Nothing to do; next pass.",
                error.map_or_else(String::new, |error| format!(" ({error})"))
            ),
        ),
        "steward_error" => NextAction::new(
            Actor::Operator,
            format!(
                "Admission failed{detail_suffix}. Nothing for the author; the operator reads the \
                 steward log."
            ),
        ),
        "required_policy_unknown" => NextAction::new(
            Actor::Operator,
            "The required-check policy could not be read. Nothing to do; next pass.".to_owned(),
        ),
        "unclassified" => NextAction::unclassified(),
        _ => return None,
    })
}

/// A head the queue ejected and the steward will not re-enqueue. A head the
/// queue could not merge at all (`invalid_merge_commit`, `merge_conflict`)
/// needs the base merged once; any other needs a fix.
fn ejected_same_head(facts: &Facts<'_>) -> NextAction {
    let Facts {
        arg,
        detail_suffix,
        repo,
        base,
        number,
        ..
    } = facts;
    let action = if arg.starts_with("invalid_merge_commit") || arg.starts_with("merge_conflict") {
        format!(
            "The merge queue could not merge this exact head ({arg}{detail_suffix}): merge \
             {base} once (no rebase) and push once. The steward does not re-enqueue the same \
             head."
        )
    } else {
        format!(
            "The merge queue ejected this exact head ({arg}{detail_suffix}): `shipyard diagnose \
             --pr {number} --repo {repo}`, fix, push once. Do not re-enqueue the same head."
        )
    };
    NextAction::new(Actor::Author, action)
}

fn excluded_label(label: &str) -> NextAction {
    if label.eq_ignore_ascii_case(SUPERSEDED_LABEL) {
        NextAction::stop(
            Actor::Owner,
            format!(
                "Labelled {label}: replaced by another pull request. Stop; work on the successor."
            ),
        )
    } else if label.to_ascii_lowercase().starts_with(PARKED_LABEL_PREFIX) {
        NextAction::stop(
            Actor::Owner,
            format!("Labelled {label}: parked. Stop; it lands when the owner unparks it."),
        )
    } else {
        NextAction::new(
            Actor::Author,
            format!(
                "Labelled {label}, which opts out of stewardship: remove the label to let the \
                 steward admit it, or stop if that is intended."
            ),
        )
    }
}

fn merge_state(state: &str, base: &str, number: u64) -> NextAction {
    match state.to_ascii_uppercase().as_str() {
        "DIRTY" | "CONFLICTING" => NextAction::new(
            Actor::Author,
            format!(
                "Conflicts with {base}: merge {base} (never rebase or force-push), resolve, push \
                 once."
            ),
        ),
        "BEHIND" => NextAction::new(
            Actor::Author,
            format!(
                "Behind {base} on a base with no merge queue: merge {base} once (no rebase), \
                 push once."
            ),
        ),
        other => NextAction::new(
            Actor::Author,
            format!(
                "GitHub reports {other}; this base admits only CLEAN. UNSTABLE: a check that is \
                 not required is red, fix or rerun it. BLOCKED: a branch rule is unmet; read \
                 `shipyard landing --pr {number}`."
            ),
        ),
    }
}

fn held(stall: &Stall, repo: &str, base: &str, number: u64) -> NextAction {
    let (holder, link) = stall.held_by.as_ref().map_or_else(
        || ("a reviewer".to_owned(), String::new()),
        |held| {
            let link = held.id.map_or_else(String::new, |id| {
                let anchor = if held.record == "review" {
                    format!("pullrequestreview-{id}")
                } else {
                    format!("issuecomment-{id}")
                };
                format!(" (https://github.com/{repo}/pull/{number}#{anchor})")
            });
            (held.holder.clone(), link)
        },
    );
    NextAction::new(
        Actor::Author,
        format!(
            "Held head: needs a new head. HOLD by {holder} on this head{link} makes it \
             un-approvable, and any APPROVE of this same head is ignored: fix what it names \
             (with a test), merge {base} if needed (no rebase), push once, then comment \
             `repaired: <what> at <sha>`. If the HOLD is wrong, still push a new head (merge \
             {base}) and comment `dispute: <reason>`; never ask for this head to be approved."
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::merge_steward_cmd::blocked_on::HeldBy;
    use crate::merge_steward::{DEFAULT_APPROVAL_CONTEXT, RequiredCheck};

    fn policy(merge_queue: bool) -> StewardPolicy {
        StewardPolicy {
            merge_queue,
            native_auto_merge: merge_queue,
            approval_context: DEFAULT_APPROVAL_CONTEXT.to_owned(),
            required_checks: vec![
                RequiredCheck {
                    context: "macos".to_owned(),
                    app_id: Some(15368),
                },
                RequiredCheck {
                    context: "review-verdict".to_owned(),
                    app_id: Some(15368),
                },
            ],
            opt_out_label: "shipyard:no-auto-merge".to_owned(),
            provenance_blocking_labels: Vec::new(),
            managed_label: None,
            handoff_context: String::new(),
            max_transient_reruns: 1,
            ejection_guards: crate::merge_steward::EjectionGuards::default(),
        }
    }

    fn pr(merge_state: &str) -> StewardPullRequest {
        StewardPullRequest {
            number: 11364,
            head_sha: "a".repeat(40),
            head_branch: "feature".to_owned(),
            author: None,
            draft: false,
            merge_state: merge_state.to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: Vec::new(),
            checks: Vec::new(),
        }
    }

    fn stall(token: &str, detail: Option<&str>) -> Stall {
        Stall {
            blocked_on: token.to_owned(),
            blocked_detail: detail.map(str::to_owned),
            ..Stall::default()
        }
    }

    fn next(token: &str, detail: Option<&str>, merge_queue: bool, merge_state: &str) -> NextAction {
        let pr = pr(merge_state);
        let policy = policy(merge_queue);
        for_row(
            &RowContext {
                repo: "Generous-Corp/pulp",
                base: "main",
                pr: &pr,
                policy: &policy,
                error: None,
            },
            &stall(token, detail),
        )
    }

    #[test]
    fn a_hold_names_the_holder_links_the_record_and_is_the_authors_move() {
        let mut held = stall("held", None);
        held.held_by = Some(HeldBy {
            holder: "infra-review2".to_owned(),
            record: "comment",
            id: Some(6_544_000_001),
        });
        let pr = pr("CLEAN");
        let policy = policy(true);
        let next = for_row(
            &RowContext {
                repo: "Generous-Corp/pulp",
                base: "main",
                pr: &pr,
                policy: &policy,
                error: None,
            },
            &held,
        );
        assert_eq!(next.actor, Actor::Author);
        assert!(next.actionable());
        assert!(!next.stop);
        assert!(next.action.contains("HOLD by infra-review2"), "{next:?}");
        assert!(
            next.action.contains(
                "https://github.com/Generous-Corp/pulp/pull/11364#issuecomment-6544000001"
            ),
            "{next:?}"
        );
        assert!(next.action.contains("repaired:"), "{next:?}");
        assert!(
            next.action.starts_with("Held head: needs a new head."),
            "{next:?}"
        );
        assert!(next.action.contains("dispute:"), "{next:?}");
    }

    #[test]
    fn a_review_hold_links_the_review_record() {
        let mut held = stall("held", None);
        held.held_by = Some(HeldBy {
            holder: "reviewer".to_owned(),
            record: "review",
            id: Some(7),
        });
        let pr = pr("CLEAN");
        let policy = policy(true);
        let next = for_row(
            &RowContext {
                repo: "o/r",
                base: "main",
                pr: &pr,
                policy: &policy,
                error: None,
            },
            &held,
        );
        assert!(next.action.contains("#pullrequestreview-7"), "{next:?}");
    }

    #[test]
    fn green_with_the_reviewer_pending_is_nothing_to_do() {
        let next = next("no_verdict", None, true, "CLEAN");
        assert_eq!(next.actor, Actor::Reviewer);
        assert!(!next.actionable());
        assert!(
            next.action
                .contains("Awaiting the standing reviewer; nothing for the author to do"),
            "{next:?}"
        );
        assert!(next.action.contains("do not merge main"), "{next:?}");
        assert!(next.action.contains("macos,review-verdict"), "{next:?}");
    }

    #[test]
    fn a_pending_approval_context_is_the_reviewers_wait_not_a_ci_run() {
        let waiting = next("required_pending", Some("review-verdict"), true, "CLEAN");
        assert_eq!(waiting.actor, Actor::Reviewer);
        assert!(!waiting.actionable());
        assert!(
            waiting
                .action
                .contains("Awaiting the standing reviewer; nothing for the author to do"),
            "{waiting:?}"
        );
        // Control: a pending ordinary check stays CI's, and a mixed set too.
        for detail in ["macos", "macos,review-verdict"] {
            let other = next("required_pending", Some(detail), true, "CLEAN");
            assert_eq!(other.actor, Actor::Ci, "{detail}");
        }
    }

    #[test]
    fn a_failed_required_check_is_named_and_sent_to_diagnose() {
        let next = next("required_failed:macos", None, true, "CLEAN");
        assert_eq!(next.actor, Actor::Author);
        assert!(next.actionable());
        assert!(next.action.contains("macos"), "{next:?}");
        assert!(
            next.action
                .contains("shipyard diagnose --pr 11364 --repo Generous-Corp/pulp"),
            "{next:?}"
        );
        assert!(next.action.contains("not required do not gate"), "{next:?}");
    }

    #[test]
    fn pending_required_checks_say_advisory_checks_do_not_gate() {
        let next = next("required_pending", Some("macos"), true, "CLEAN");
        assert_eq!(next.actor, Actor::Ci);
        assert!(!next.actionable());
        assert!(next.action.contains("not required do not gate"), "{next:?}");
    }

    #[test]
    fn behind_with_a_merge_queue_says_do_not_merge_main() {
        let next = next("no_verdict", None, true, "BEHIND");
        assert!(!next.actionable());
        assert!(
            next.action
                .contains("do not merge main: the merge queue brings it up to date"),
            "{next:?}"
        );
        // The author's own moves never carry the suffix: a conflict is still
        // resolved by merging main.
        let conflict = next_fn("merge_state:DIRTY", true, "DIRTY");
        assert!(
            !conflict.action.contains("merge queue brings"),
            "{conflict:?}"
        );
    }

    fn next_fn(token: &str, merge_queue: bool, merge_state: &str) -> NextAction {
        next(token, None, merge_queue, merge_state)
    }

    #[test]
    fn behind_without_a_merge_queue_is_one_merge_of_the_base() {
        let next = next("merge_state:BEHIND", None, false, "BEHIND");
        assert_eq!(next.actor, Actor::Author);
        assert!(
            next.action.contains("merge main once (no rebase)"),
            "{next:?}"
        );
    }

    #[test]
    fn a_conflict_is_resolved_by_merging_never_rebasing() {
        let next = next("merge_state:DIRTY", None, true, "DIRTY");
        assert_eq!(next.actor, Actor::Author);
        assert!(
            next.action.contains("never rebase or force-push"),
            "{next:?}"
        );
    }

    #[test]
    fn owner_decision_and_fleet_hold_labels_stop_the_caller() {
        for (token, needle) in [
            ("owner_decision", "owner decides"),
            ("excluded_label:superseded", "successor"),
            ("excluded_label:PARKED/windows", "parked"),
            ("provenance_blocked:5·unresolved", "provenance"),
            ("direct_merge_refused", "maintainer merges"),
        ] {
            let next = next(token, None, true, "CLEAN");
            assert_eq!(next.actor, Actor::Owner, "{token}");
            assert!(next.stop, "{token}");
            assert!(next.actionable(), "{token}");
            assert!(next.action.contains(needle), "{token}: {next:?}");
        }
    }

    #[test]
    fn the_opt_out_label_is_the_authors_to_remove() {
        let next = next("excluded_label:shipyard:no-auto-merge", None, true, "CLEAN");
        assert_eq!(next.actor, Actor::Author);
        assert!(!next.stop);
        assert!(next.action.contains("remove the label"), "{next:?}");
    }

    #[test]
    fn queued_ready_and_admitted_rows_have_nothing_to_do() {
        for token in [
            "queued",
            "ready",
            "admitted",
            "transient_rerun",
            "revalidating",
        ] {
            let next = next(token, Some("position 1"), true, "CLEAN");
            assert!(!next.actionable(), "{token}: {next:?}");
            assert!(next.action.contains("Nothing to do"), "{token}: {next:?}");
        }
        assert_eq!(next("admitted", None, true, "CLEAN").actor, Actor::Nobody);
    }

    #[test]
    fn an_ejected_head_is_the_authors_and_never_re_enqueued() {
        let next = next(
            "ejected_same_head:failed_checks",
            Some("job macos failed; retry spent"),
            true,
            "CLEAN",
        );
        assert_eq!(next.actor, Actor::Author);
        assert!(next.action.contains("failed_checks"), "{next:?}");
        assert!(next.action.contains("retry spent"), "{next:?}");
        assert!(next.action.contains("Do not re-enqueue"), "{next:?}");
    }

    #[test]
    fn an_unmergeable_ejected_head_needs_one_merge_of_the_base() {
        let next = next(
            "ejected_same_head:invalid_merge_commit:needs_new_head",
            None,
            true,
            "CLEAN",
        );
        assert_eq!(next.actor, Actor::Author);
        assert!(next.action.contains("merge main once"), "{next:?}");
        assert!(next.action.contains("does not re-enqueue"), "{next:?}");
    }

    #[test]
    fn a_generated_path_batch_waits_on_the_steward() {
        let next = next(
            "generated_path_batch:#11720",
            Some("test/ctest_script_inputs.json"),
            true,
            "CLEAN",
        );
        assert_eq!(next.actor, Actor::Steward);
        assert!(!next.actionable(), "{next:?}");
        assert!(next.action.contains("#11720"), "{next:?}");
        assert!(next.action.contains("Nothing to do"), "{next:?}");
    }

    #[test]
    fn carried_commits_name_the_other_pull_request() {
        let next = next(
            "carries_unapproved_commits:#10",
            Some("abc123"),
            true,
            "CLEAN",
        );
        assert_eq!(next.actor, Actor::Author);
        assert!(next.action.contains("#10's approval"), "{next:?}");
    }

    #[test]
    fn a_check_that_never_ran_is_restarted_by_one_merge_of_the_base() {
        let next = next("required_never_ran:macos", None, true, "CLEAN");
        assert_eq!(next.actor, Actor::Author);
        assert!(next.action.contains("merge main once"), "{next:?}");
    }

    #[test]
    fn operator_tokens_keep_the_caller_waiting() {
        for token in [
            "approval_unread",
            "steward_error",
            "required_policy_unknown",
            "carried_commits_unreadable",
            "invalid_head",
            "unclassified",
            "something_new",
        ] {
            let next = next(token, Some("boom"), true, "CLEAN");
            assert_eq!(next.actor, Actor::Operator, "{token}");
            assert!(!next.actionable(), "{token}");
        }
    }

    #[test]
    fn terminal_and_single_pr_constructors() {
        let merged = NextAction::merged("feature", "main", Some("2026-11-01T10:00:00Z"));
        assert_eq!(merged.actor, Actor::Nobody);
        assert!(merged.stop && merged.actionable());
        assert!(merged.action.contains("retargeted to `main`"), "{merged:?}");
        assert!(merged.action.contains("2026-11-01T10:00:00Z"), "{merged:?}");

        let closed = NextAction::closed();
        assert!(closed.stop);

        let waiting = NextAction::stacked("feat/parent", "main", Some(7));
        assert_eq!(waiting.actor, Actor::Steward);
        assert!(!waiting.actionable());
        assert!(
            waiting.action.contains("shipyard wait pr 7 --state merged"),
            "{waiting:?}"
        );

        let orphan = NextAction::stacked("feat/parent", "main", None);
        assert_eq!(orphan.actor, Actor::Author);
        assert!(
            orphan
                .action
                .contains("retarget this pull request to `main` now"),
            "{orphan:?}"
        );

        let empty = NextAction::empty_diff("main");
        assert_eq!(empty.actor, Actor::Author);
        assert!(empty.action.contains("0 changed files"), "{empty:?}");
        assert_eq!(next("empty_diff", None, true, "CLEAN"), empty);
    }

    #[test]
    fn json_uses_snake_case_actors() {
        let value = serde_json::to_value(next("no_verdict", None, true, "CLEAN")).expect("json");
        assert_eq!(value["actor"], "reviewer");
        assert_eq!(value["stop"], false);
        assert!(value["action"].is_string());
    }
}
