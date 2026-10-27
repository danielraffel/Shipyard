//! Repository-aware batching guidance.

use serde::Serialize;

use super::Verdict;
use super::queue::{MergeQueueConfig, QueueFinding};
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
    /// Suggested guarded command, when actionable.
    pub command: Option<String>,
    /// Configured upper bound for a rollup.
    pub max_members: Option<usize>,
    /// Evidence-based explanation.
    pub rationale: String,
}

fn queue_is_enforcing(queue: &QueueFinding) -> Option<&MergeQueueConfig> {
    match &queue.verdict {
        Verdict::Present(config) if config.enforcing() => Some(config),
        _ => None,
    }
}

/// Select a batching strategy from measured queue state and repository config.
#[must_use]
pub fn batching_guidance(queue: &QueueFinding, train: &Config) -> BatchingGuidance {
    if queue.verdict.is_unknown() {
        return BatchingGuidance {
            action: BatchingAction::Unknown,
            command: None,
            max_members: (train.enabled && train.mode == Mode::Rollup)
                .then_some(train.max_members),
            rationale: "The repository's merge-queue state is unreadable; do not recommend a landing or batching command until GitHub queue authority can be measured.".to_owned(),
        };
    }
    if !train.enabled || train.mode != Mode::Rollup {
        return if queue_is_enforcing(queue).is_some() {
            BatchingGuidance { action: BatchingAction::QueueOnly, command: Some("shipyard ship --pr <number>".to_owned()), max_members: None, rationale: "No rollup train is enabled; use the repository's merge queue, which can batch merge groups itself.".to_owned() }
        } else {
            BatchingGuidance { action: BatchingAction::DirectOnly, command: Some("shipyard auto-merge <number>".to_owned()), max_members: None, rationale: "No rollup train is enabled; validated pull requests land through the repository's direct merge path.".to_owned() }
        };
    }
    let command = Some(
        "shipyard rollup plan && shipyard rollup build --members <n,n,...> --apply".to_owned(),
    );
    match (queue_is_enforcing(queue), train.landing) {
        (Some(_), Landing::MergeQueue) => BatchingGuidance { action: BatchingAction::RollupThenEnqueue, command, max_members: Some(train.max_members), rationale: format!("The repository enables a guarded rollup train and its merge queue is enforcing; form up to {} compatible approved members, then enqueue the rollup PR so GitHub validates the combined head.", train.max_members) },
        (None, Landing::Direct) => BatchingGuidance { action: BatchingAction::RollupThenDirect, command, max_members: Some(train.max_members), rationale: format!("The repository has no enforcing merge queue and explicitly selects direct rollup landing; form up to {} compatible approved members and use the guarded direct landing after the rollup gate passes.", train.max_members) },
        (None, Landing::MergeQueue) => BatchingGuidance { action: BatchingAction::Unknown, command: None, max_members: Some(train.max_members), rationale: "The rollup train requests merge-queue landing, but no enforcing queue was measured; do not form or merge a batch until the repository config and GitHub rules agree.".to_owned() },
        (Some(_), Landing::Direct) => BatchingGuidance { action: BatchingAction::Unknown, command: None, max_members: Some(train.max_members), rationale: "The rollup train requests direct landing while an enforcing merge queue is present; refuse to guess which authority should land the batch.".to_owned() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fleet_service::Boundary;
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
}
