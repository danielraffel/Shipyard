//! Repository-aware batching guidance.

use serde::Serialize;

use super::Verdict;
use super::queue::QueueFinding;
use crate::rollup::{Config, Landing, Mode};

/// The safe next landing shape for this repository.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BatchingAction {
    /// No batch train is active.
    Disabled,
    /// Form a rollup, then enqueue it into the enforcing queue.
    RollupThenEnqueue,
    /// Form a rollup, then use guarded direct landing.
    RollupThenDirect,
    /// Use the native merge queue without a Shipyard rollup.
    QueueOnly,
    /// Land validated pull requests directly without batching.
    DirectOnly,
    /// Configured train and measured GitHub state disagree.
    Unknown,
}

impl BatchingAction {
    /// Stable human-readable action label.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::RollupThenEnqueue => "rollup_then_enqueue",
            Self::RollupThenDirect => "rollup_then_direct",
            Self::QueueOnly => "queue_only",
            Self::DirectOnly => "direct_only",
            Self::Unknown => "unknown",
        }
    }
}

/// Action and command for batching the repository's current backlog.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct BatchingGuidance {
    /// Selected strategy.
    pub action: BatchingAction,
    /// Suggested read-only command, when actionable.
    pub command: Option<String>,
    /// Configured upper bound for a rollup.
    pub max_members: Option<usize>,
    /// Evidence-based explanation.
    pub rationale: String,
}

/// Return the queue's enforcement state only when GitHub supplied the field.
/// GraphQL's effective queue payload deliberately omits it; treating that
/// omission as enforcing would turn an incomplete observation into an enqueue
/// recommendation.
fn queue_enforcement(queue: &QueueFinding) -> Option<bool> {
    match &queue.verdict {
        Verdict::Present(config) => match config.enforcement.as_deref() {
            Some(value) if value.eq_ignore_ascii_case("active") => Some(true),
            Some(value)
                if value.eq_ignore_ascii_case("disabled")
                    || value.eq_ignore_ascii_case("evaluate") =>
            {
                Some(false)
            }
            _ => None,
        },
        Verdict::Absent => Some(false),
        Verdict::Unknown { .. } => None,
    }
}

/// Select a batching strategy from measured queue state and repository config.
#[must_use]
pub fn batching_guidance(queue: &QueueFinding, train: &Config) -> BatchingGuidance {
    let enforcing = queue_enforcement(queue);
    if enforcing.is_none() {
        return BatchingGuidance {
            action: BatchingAction::Unknown,
            command: None,
            max_members: (train.enabled && train.mode == Mode::Rollup)
                .then_some(train.max_members),
            rationale: "The repository's merge-queue state is unreadable; do not recommend a landing or batching command until GitHub queue authority can be measured.".to_owned(),
        };
    }
    if !train.enabled || train.mode != Mode::Rollup {
        return if enforcing == Some(true) {
            BatchingGuidance { action: BatchingAction::QueueOnly, command: Some("shipyard landing --pr <number>".to_owned()), max_members: None, rationale: "No rollup train is enabled; use the repository's merge queue, which can batch merge groups itself. This report is read-only and never enqueues.".to_owned() }
        } else {
            BatchingGuidance { action: BatchingAction::DirectOnly, command: Some("shipyard landing --pr <number>".to_owned()), max_members: None, rationale: "No rollup train is enabled; validated pull requests land through the repository's direct merge path. This report is read-only and never merges.".to_owned() }
        };
    }
    // This report is read-only. It may point at the read-only plan command,
    // but never emits a push, enqueue, or merge command.
    let command = Some("shipyard rollup plan".to_owned());
    match (enforcing, train.landing) {
        (Some(true), Landing::MergeQueue) => BatchingGuidance { action: BatchingAction::RollupThenEnqueue, command, max_members: Some(train.max_members), rationale: format!("The repository enables a guarded rollup train and its merge queue is enforcing; form up to {} compatible approved members, then enqueue the rollup PR so GitHub validates the combined head.", train.max_members) },
        (Some(false), Landing::Direct) => BatchingGuidance { action: BatchingAction::RollupThenDirect, command, max_members: Some(train.max_members), rationale: format!("The repository has no enforcing merge queue and explicitly selects direct rollup landing; form up to {} compatible approved members and use the guarded direct landing after the rollup gate passes.", train.max_members) },
        (Some(false), Landing::MergeQueue) => BatchingGuidance { action: BatchingAction::Unknown, command: None, max_members: Some(train.max_members), rationale: "The rollup train requests merge-queue landing, but no enforcing queue was measured; do not form or merge a batch until the repository config and GitHub rules agree.".to_owned() },
        (Some(true), Landing::Direct) => BatchingGuidance { action: BatchingAction::Unknown, command: None, max_members: Some(train.max_members), rationale: "The rollup train requests direct landing while an enforcing merge queue is present; refuse to guess which authority should land the batch.".to_owned() },
        (None, _) => unreachable!("unknown enforcement was returned before matching"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_service::Boundary;
    use crate::landing::queue::MergeQueueConfig;
    fn queue(active: Option<bool>) -> QueueFinding {
        QueueFinding {
            verdict: active.map_or(Verdict::Absent, |on| {
                Verdict::Present(MergeQueueConfig {
                    enforcement: Some(if on { "active" } else { "disabled" }.to_owned()),
                    ..Default::default()
                })
            }),
            disagreements: Vec::new(),
        }
    }
    #[test]
    fn rollup_queue_is_two_stage() {
        let c = Config {
            enabled: true,
            mode: Mode::Rollup,
            ..Config::default()
        };
        assert_eq!(
            batching_guidance(&queue(Some(true)), &c).action,
            BatchingAction::RollupThenEnqueue
        );
    }
    #[test]
    fn queue_landing_without_queue_fails_closed() {
        let c = Config {
            enabled: true,
            mode: Mode::Rollup,
            ..Config::default()
        };
        assert_eq!(
            batching_guidance(&queue(None), &c).action,
            BatchingAction::Unknown
        );
    }

    fn unknown_queue() -> QueueFinding {
        QueueFinding {
            verdict: Verdict::Unknown {
                boundary: Boundary::Transport,
                detail: "rulesets unavailable".to_owned(),
            },
            disagreements: Vec::new(),
        }
    }

    #[test]
    fn unreadable_queue_with_train_disabled_has_no_command() {
        let guidance = batching_guidance(&unknown_queue(), &Config::default());
        assert_eq!(guidance.action, BatchingAction::Unknown);
        assert_eq!(guidance.command, None);
    }

    #[test]
    fn unreadable_queue_with_direct_rollup_has_no_command() {
        let c = Config {
            enabled: true,
            mode: Mode::Rollup,
            landing: Landing::Direct,
            ..Config::default()
        };
        let guidance = batching_guidance(&unknown_queue(), &c);
        assert_eq!(guidance.action, BatchingAction::Unknown);
        assert_eq!(guidance.command, None);
        assert_eq!(guidance.max_members, Some(c.max_members));
    }

    #[test]
    fn queue_without_enforcement_is_unknown_and_non_mutating() {
        let queue = QueueFinding {
            verdict: Verdict::Present(crate::landing::queue::MergeQueueConfig {
                enforcement: None,
                ..Default::default()
            }),
            disagreements: Vec::new(),
        };
        let c = Config {
            enabled: true,
            mode: Mode::Rollup,
            ..Config::default()
        };
        let guidance = batching_guidance(&queue, &c);
        assert_eq!(guidance.action, BatchingAction::Unknown);
        assert_eq!(guidance.command, None);
    }

    #[test]
    fn evaluate_only_queue_is_non_enforcing() {
        let queue = QueueFinding {
            verdict: Verdict::Present(MergeQueueConfig {
                enforcement: Some("evaluate".to_owned()),
                merge_method: Some("MERGE".to_owned()),
                ..Default::default()
            }),
            disagreements: Vec::new(),
        };
        let c = Config {
            enabled: true,
            mode: Mode::Rollup,
            landing: Landing::Direct,
            ..Config::default()
        };
        let guidance = batching_guidance(&queue, &c);
        assert_eq!(guidance.action, BatchingAction::RollupThenDirect);
        assert_eq!(guidance.command.as_deref(), Some("shipyard rollup plan"));
    }

    #[test]
    fn actionable_guidance_never_emits_a_mutating_command() {
        let c = Config {
            enabled: true,
            mode: Mode::Rollup,
            ..Config::default()
        };
        let guidance = batching_guidance(&queue(Some(true)), &c);
        assert_eq!(guidance.command.as_deref(), Some("shipyard rollup plan"));
        assert!(!guidance.command.unwrap().contains("--apply"));
    }
}
