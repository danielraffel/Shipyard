//! Decide whether GitHub-native auto-merge may be armed on one pull request.
//!
//! ## Why native auto-merge at all, when Shipyard already enqueues
//!
//! Shipyard's own admission path (`shipyard auto-merge`, and the steward's
//! `enqueuePullRequest`) enqueues a head *after* Shipyard has validated it.
//! That is strictly stronger than native auto-merge — and strictly less
//! durable, because it only happens while a Shipyard process is alive to do
//! it. A pull request whose ship lost its merge phase (the session died, the
//! daemon dropped the job, the run was opened by another route) is then green,
//! unqueued, and unarmed, and nothing on GitHub's side will ever move it.
//!
//! `enablePullRequestAutoMerge` is server-owned: once armed, GitHub itself
//! enqueues the pull request when its required checks pass, with no local
//! process involved. Arming it is therefore a *backstop* under Shipyard's
//! validated enqueue, not a replacement for it.
//!
//! ## Why the merge method is always `MERGE`
//!
//! A squash folds every commit subject into the squash subject, including the
//! `chore: bump versions` marker commit. Release automation reads that subject
//! to decide whether a release was cut, so a squashed bump marker trips a
//! false release alarm. `MERGE` preserves the marker as its own commit.
//!
//! ## What this module is not
//!
//! It performs no I/O. Callers supply already-read facts and receive a verdict
//! plus the exact reason, so every refusal can be reported verbatim without
//! the caller re-deriving policy. Transport lives in
//! [`crate::app::ship_cmd`] (arm-on-open) and
//! [`crate::app::merge_steward_cmd`] (the periodic backstop).

use crate::merge_steward::StewardPullRequest;
use crate::pr_queue_state::{PrQueueState, same_head_requeue_allowed};

/// GraphQL document that arms native auto-merge. Variable: `id`.
///
/// `MERGE` is deliberate and load-bearing; see the module docs.
pub const NATIVE_AUTO_MERGE_MUTATION: &str = "mutation($id:ID!){enablePullRequestAutoMerge(input:\
     {pullRequestId:$id,mergeMethod:MERGE}){pullRequest{number}}}";

/// `mergeStateStatus` values on which arming native auto-merge is appropriate.
///
/// Read as "GitHub does not currently know of anything on this pull request
/// that a merge queue would not absorb":
///
/// * `CLEAN` — mergeable, required checks pass.
/// * `UNSTABLE` — a *non-required* check is failing; still mergeable.
/// * `BEHIND` — merely out of date with the base, which is exactly what a
///   queue exists to absorb. Refusing these would exclude the majority of a
///   backlog under up-to-date protection, where every landing puts the rest
///   behind.
/// * `HAS_HOOKS` — mergeable with pre-receive hooks configured.
///
/// Everything else is refused, in particular `BLOCKED` (a required check is
/// failing, missing, or a review is outstanding) and `DIRTY` / `CONFLICTING`
/// (the author must resolve conflicts first). `UNKNOWN` is GitHub still
/// computing mergeability and is never read as permission.
const ARM_READY_MERGE_STATES: &[&str] = &["CLEAN", "UNSTABLE", "BEHIND", "HAS_HOOKS"];

/// Whether `merge_state` is a GitHub mergeability verdict that permits arming.
///
/// Case-insensitive: GitHub returns upper case, but a replayed fixture or a
/// hand-written config may not.
#[must_use]
pub fn merge_state_is_arm_ready(merge_state: &str) -> bool {
    ARM_READY_MERGE_STATES
        .iter()
        .any(|ready| merge_state.eq_ignore_ascii_case(ready))
}

/// Why arming was declined. Every variant is a normal outcome, not a fault.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "skip", rename_all = "snake_case")]
pub enum ArmSkip {
    /// Native auto-merge is already armed. Arming again is a no-op request
    /// that only spends quota.
    AlreadyArmed {
        /// `autoMergeRequest.enabledAt`, when known.
        enabled_at: Option<String>,
    },
    /// Already in the merge queue. GitHub consumed the auto-merge request on
    /// admission, so a `null` `autoMergeRequest` here means *queued*, never
    /// *unarmed* — re-arming would re-enqueue a head the queue already holds.
    AlreadyQueued {
        /// Zero-based queue position, when known.
        position: Option<u64>,
    },
    /// GitHub refuses auto-merge on a draft.
    Draft,
    /// Already merged.
    Merged,
    /// Closed without merging.
    Closed,
    /// The queue ejected this exact head and no new head followed. Re-arming
    /// re-enqueues it, and under `ALLGREEN` grouping that fails every
    /// batch-mate with it.
    EjectedSameHead {
        /// `RemovedFromMergeQueueEvent.reason` of the last removal.
        reason: String,
    },
    /// GitHub's mergeability verdict does not permit arming yet.
    NotArmReady {
        /// The observed `mergeStateStatus`.
        merge_state: String,
    },
    /// The pull request carries the configured opt-out label.
    OptedOut {
        /// The label that opted it out.
        label: String,
    },
    /// The repository does not allow native auto-merge at all.
    NativeAutoMergeDisabled,
    /// The state could not be determined. Never treated as "unarmed".
    Unknown {
        /// What was missing or malformed.
        detail: String,
    },
}

impl ArmSkip {
    /// One line a human or agent can read without consulting this module.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            Self::AlreadyArmed { enabled_at } => format!(
                "auto-merge is already armed{}; the queue admits it once required checks pass",
                enabled_at
                    .as_deref()
                    .map_or_else(String::new, |at| format!(" since {at}"))
            ),
            Self::AlreadyQueued { position } => format!(
                "already in the merge queue{}; re-arming would re-enqueue a head the queue \
                 already holds",
                position.map_or_else(String::new, |position| format!(" at position {position}"))
            ),
            Self::Draft => "it is a draft, and GitHub refuses auto-merge on drafts".to_owned(),
            Self::Merged => "it is already merged".to_owned(),
            Self::Closed => "it is closed".to_owned(),
            Self::EjectedSameHead { reason } => format!(
                "the queue ejected this exact head ({reason}) and no new head followed; \
                 re-arming would re-enqueue it and fail its batch-mates"
            ),
            Self::NotArmReady { merge_state } => format!(
                "GitHub reports mergeStateStatus={merge_state}, which is not a state that \
                 permits arming"
            ),
            Self::OptedOut { label } => format!("it carries the opt-out label `{label}`"),
            Self::NativeAutoMergeDisabled => {
                "the repository does not allow native auto-merge".to_owned()
            }
            Self::Unknown { detail } => format!(
                "its merge-queue state could not be determined ({detail}); refusing to arm blind"
            ),
        }
    }
}

/// Whether to arm native auto-merge on one pull request.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ArmVerdict {
    /// Arm it.
    Arm,
    /// Leave it alone, for this reason.
    Skip(ArmSkip),
}

impl ArmVerdict {
    /// Whether this verdict authorizes the mutation.
    #[must_use]
    pub const fn arms(&self) -> bool {
        matches!(self, Self::Arm)
    }
}

/// Decide from an authoritative [`PrQueueState`] plus the draft flag.
///
/// This is the arm-on-open path's decision: the classification has read the
/// pull request's timeline, so it can tell a never-armed pull request from an
/// ejected one, which the cheap backlog row below cannot.
///
/// `draft` is a separate argument because [`PrQueueState`] deliberately does
/// not carry it: its GraphQL document is mirrored by
/// `scripts/ghapp_queue_arm_guard.py`, and the two must stay in lockstep.
#[must_use]
pub fn decide_from_queue_state(state: &PrQueueState, draft: bool) -> ArmVerdict {
    // Draft is checked first among the open states but *after* the terminal
    // ones: a merged pull request that was once a draft is merged, not a
    // draft, and reporting it as a draft would read as something to fix.
    match state {
        PrQueueState::Merged => ArmVerdict::Skip(ArmSkip::Merged),
        PrQueueState::Closed => ArmVerdict::Skip(ArmSkip::Closed),
        PrQueueState::Queued { position, .. } => ArmVerdict::Skip(ArmSkip::AlreadyQueued {
            position: *position,
        }),
        PrQueueState::ArmedNotQueued { enabled_at, .. } => {
            ArmVerdict::Skip(ArmSkip::AlreadyArmed {
                enabled_at: enabled_at.clone(),
            })
        }
        PrQueueState::Unknown { detail } => ArmVerdict::Skip(ArmSkip::Unknown {
            detail: detail.clone(),
        }),
        PrQueueState::Ejected {
            reason,
            new_head_since_removal,
            ..
        } => {
            if draft {
                return ArmVerdict::Skip(ArmSkip::Draft);
            }
            // A new head since the removal is a different head, so the
            // ejection says nothing against it. `invalid_merge_commit` is the
            // one reason that says nothing against the head even unchanged:
            // GitHub failed to build the merge commit.
            if *new_head_since_removal || same_head_requeue_allowed(reason) {
                ArmVerdict::Arm
            } else {
                ArmVerdict::Skip(ArmSkip::EjectedSameHead {
                    reason: reason.clone(),
                })
            }
        }
        PrQueueState::NeverArmed => {
            if draft {
                ArmVerdict::Skip(ArmSkip::Draft)
            } else {
                ArmVerdict::Arm
            }
        }
    }
}

/// Cheap pre-filter over one already-observed backlog row.
///
/// The periodic backstop reads every open pull request once, so this decides
/// from facts that read costs nothing: draft, queue position, armed flag,
/// labels, and GitHub's own `mergeStateStatus`. It deliberately returns
/// [`ArmVerdict::Arm`] for rows that merely *look* unarmed — the caller must
/// then confirm each candidate against an authoritative [`PrQueueState`] via
/// [`decide_from_queue_state`] before mutating, because a backlog row carries
/// no timeline and so cannot distinguish never-armed from ejected.
///
/// `native_auto_merge` is the repository's `allow_auto_merge` setting.
#[must_use]
pub fn preselect_backstop_candidate(
    pr: &StewardPullRequest,
    native_auto_merge: bool,
    opt_out_label: &str,
) -> ArmVerdict {
    if !native_auto_merge {
        return ArmVerdict::Skip(ArmSkip::NativeAutoMergeDisabled);
    }
    if let Some(label) = pr
        .labels
        .iter()
        .find(|label| label.eq_ignore_ascii_case(opt_out_label))
    {
        return ArmVerdict::Skip(ArmSkip::OptedOut {
            label: label.clone(),
        });
    }
    // Queue membership is read from the queue's own entries, never from a
    // null auto-merge request, so this is authoritative even though the row
    // has no timeline.
    if let Some(position) = pr.queue_position {
        return ArmVerdict::Skip(ArmSkip::AlreadyQueued {
            position: Some(position),
        });
    }
    if pr.auto_merge_active {
        return ArmVerdict::Skip(ArmSkip::AlreadyArmed { enabled_at: None });
    }
    if pr.draft {
        return ArmVerdict::Skip(ArmSkip::Draft);
    }
    if !merge_state_is_arm_ready(&pr.merge_state) {
        return ArmVerdict::Skip(ArmSkip::NotArmReady {
            merge_state: pr.merge_state.clone(),
        });
    }
    ArmVerdict::Arm
}

/// `gh api graphql` arguments that arm native auto-merge on `node_id`.
#[must_use]
pub fn arm_mutation_args(node_id: &str) -> Vec<String> {
    vec![
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={NATIVE_AUTO_MERGE_MUTATION}"),
        "-F".to_owned(),
        format!("id={node_id}"),
    ]
}

/// Whether a mutation response proves a pull request came back armed.
///
/// GitHub answers a *rejected* GraphQL mutation with HTTP 200 plus an `errors`
/// array, so a successful exit status proves nothing. Both conditions are
/// load-bearing and neither implies the other: a partial success carries the
/// mutation payload *and* an `errors` array, and a hard rejection carries a
/// null payload with no payload key at all.
#[must_use]
pub fn arm_response_accepted(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw).is_ok_and(|value| {
        value.get("errors").is_none()
            && value
                .pointer("/data/enablePullRequestAutoMerge/pullRequest")
                .is_some_and(|pull_request| !pull_request.is_null())
    })
}

/// The first GraphQL error message in a response, for reporting.
#[must_use]
pub fn first_graphql_error(raw: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("errors")?
                .as_array()?
                .first()?
                .get("message")?
                .as_str()
                .map(str::to_owned)
        })
}

/// Whether a failed arm attempt was the `ghapp` arm guard declining it.
///
/// The guard refuses exactly the states this module also refuses, so its
/// refusal is agreement, not a fault: it must be reported and stepped over,
/// never retried and never overridden. `GHAPP_ALLOW_QUEUE_REARM` exists for
/// an operator and must never be set by Shipyard.
#[must_use]
pub fn is_arm_guard_refusal(message: &str) -> bool {
    message.contains("queue-arm-guard:")
}

#[cfg(test)]
mod tests;
