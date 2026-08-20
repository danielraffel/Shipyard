//! Versioned, deterministic schemas for durable PR stewardship.
//!
//! This module is deliberately pure: it validates and replays records but has
//! no filesystem, GitHub, runner, queue, or cleanup side effects.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, de};

/// Current schema version for work items, transitions, and receipts.
pub const CONTROL_PLANE_SCHEMA_VERSION: u32 = 1;

/// Immutable identity of one pull-request head.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemKey {
    /// Canonical `owner/name` repository slug.
    pub repository: String,
    /// GitHub pull-request number.
    pub pull_request: u64,
    /// Exact Git commit object ID for this work item.
    pub head_sha: String,
}

/// Durable lifecycle state of one exact-head work item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemState {
    /// Exact-head handoff was accepted but has not yet been observed.
    Accepted,
    /// The controller is reconciling repository and check state.
    Observing,
    /// A bounded repair has been requested for this exact head.
    RepairRequested,
    /// Required checks are incomplete.
    AwaitingChecks,
    /// GitHub accepted this exact head into its server-owned merge queue.
    Queued,
    /// GitHub reports the exact work item terminally merged.
    Merged,
    /// GitHub reports the pull request terminally closed without merge.
    Closed,
    /// A newer exact head replaced this work item.
    Superseded,
    /// Stewardship authority for this work item was explicitly revoked.
    Revoked,
}

impl WorkItemState {
    /// Whether no later transition may legally originate from this state.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Merged | Self::Closed | Self::Superseded | Self::Revoked
        )
    }
}

/// Current durable state for one exact-head work item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItem {
    /// Schema version.
    pub schema_version: u32,
    /// Immutable repository, PR, and exact-head identity.
    pub key: WorkItemKey,
    /// Monotonically increasing generation, starting at one.
    pub generation: u64,
    /// Every accepted delivery identity, retained across checkpoints.
    pub accepted_idempotency_keys: BTreeSet<String>,
    /// Current lifecycle state.
    pub state: WorkItemState,
    /// Replacement exact-head identity, present exactly when superseded.
    pub superseded_by: Option<WorkItemKey>,
}

/// One proposed state transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemTransition {
    /// Schema version.
    pub schema_version: u32,
    /// Exact work-item identity; it must match the current item.
    pub key: WorkItemKey,
    /// Generation this transition creates.
    pub generation: u64,
    /// Stable delivery identity used to reject duplicate application.
    pub idempotency_key: String,
    /// State the producer observed before proposing this transition.
    pub from: WorkItemState,
    /// Proposed new state.
    pub to: WorkItemState,
    /// Replacement exact-head identity, required only for `superseded`.
    pub superseded_by: Option<WorkItemKey>,
}

/// Durable acknowledgement of one accepted transition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkItemReceipt {
    /// Schema version.
    pub schema_version: u32,
    /// Exact work-item identity.
    pub key: WorkItemKey,
    /// Accepted generation.
    pub generation: u64,
    /// Accepted delivery identity.
    pub idempotency_key: String,
    /// Previous state.
    pub from: WorkItemState,
    /// Accepted state.
    pub to: WorkItemState,
    /// Whether the accepted state is terminal.
    pub terminal: bool,
    /// Replacement exact-head identity for a supersession receipt.
    pub superseded_by: Option<WorkItemKey>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkItemReceiptWire {
    schema_version: u32,
    key: WorkItemKey,
    generation: u64,
    idempotency_key: String,
    from: WorkItemState,
    to: WorkItemState,
    terminal: bool,
    superseded_by: Option<WorkItemKey>,
}

impl<'de> Deserialize<'de> for WorkItemReceipt {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = WorkItemReceiptWire::deserialize(deserializer)?;
        let receipt = Self {
            schema_version: wire.schema_version,
            key: wire.key,
            generation: wire.generation,
            idempotency_key: wire.idempotency_key,
            from: wire.from,
            to: wire.to,
            terminal: wire.terminal,
            superseded_by: wire.superseded_by,
        };
        validate_receipt(&receipt).map_err(de::Error::custom)?;
        Ok(receipt)
    }
}

/// Result of replaying an ordered transition corpus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplayResult {
    /// Final reconstructed work item.
    pub item: WorkItem,
    /// Receipts in deterministic input order.
    pub receipts: Vec<WorkItemReceipt>,
}

/// Fail-closed schema or state-machine rejection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaError {
    /// A record uses an unsupported schema version.
    UnsupportedSchemaVersion {
        /// Version found in the rejected record.
        found: u32,
    },
    /// A key does not contain a canonical repository, positive PR, and exact SHA.
    InvalidWorkItemKey,
    /// A generation is zero or did not advance exactly once.
    StaleGeneration {
        /// Only generation that could legally follow durable state.
        expected: u64,
        /// Generation carried by the rejected record.
        found: u64,
    },
    /// An idempotency key is empty.
    EmptyIdempotencyKey,
    /// A delivery identity was already accepted during replay.
    DuplicateIdempotencyKey {
        /// Delivery identity that was already accepted.
        key: String,
    },
    /// The transition targets a different immutable work item.
    WorkItemKeyMismatch,
    /// The producer's prior state does not match durable state.
    StateMismatch {
        /// State reconstructed from durable records.
        expected: WorkItemState,
        /// Prior state claimed by the rejected transition.
        found: WorkItemState,
    },
    /// The requested state-machine edge is not legal.
    IllegalTransition {
        /// Current durable state.
        from: WorkItemState,
        /// State requested by the rejected transition.
        to: WorkItemState,
    },
    /// A supersession target is absent, unchanged, or belongs to another PR.
    InvalidSupersession,
    /// A non-supersession transition carried a replacement key.
    UnexpectedSupersession,
}

impl fmt::Display for SchemaError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for SchemaError {}

fn apply_transition(
    item: &mut WorkItem,
    transition: &WorkItemTransition,
) -> Result<WorkItemReceipt, SchemaError> {
    validate_item(item)?;
    validate_schema_version(transition.schema_version)?;
    validate_key(&transition.key)?;
    validate_idempotency_key(&transition.idempotency_key)?;

    if item
        .accepted_idempotency_keys
        .contains(&transition.idempotency_key)
    {
        return Err(SchemaError::DuplicateIdempotencyKey {
            key: transition.idempotency_key.clone(),
        });
    }

    if transition.key != item.key {
        return Err(SchemaError::WorkItemKeyMismatch);
    }
    let expected_generation =
        item.generation
            .checked_add(1)
            .ok_or(SchemaError::StaleGeneration {
                expected: item.generation,
                found: transition.generation,
            })?;
    if transition.generation != expected_generation {
        return Err(SchemaError::StaleGeneration {
            expected: expected_generation,
            found: transition.generation,
        });
    }
    if transition.from != item.state {
        return Err(SchemaError::StateMismatch {
            expected: item.state,
            found: transition.from,
        });
    }
    if !legal_transition(transition.from, transition.to) {
        return Err(SchemaError::IllegalTransition {
            from: transition.from,
            to: transition.to,
        });
    }
    validate_supersession(transition)?;

    let receipt = WorkItemReceipt {
        schema_version: CONTROL_PLANE_SCHEMA_VERSION,
        key: item.key.clone(),
        generation: transition.generation,
        idempotency_key: transition.idempotency_key.clone(),
        from: transition.from,
        to: transition.to,
        terminal: transition.to.is_terminal(),
        superseded_by: transition.superseded_by.clone(),
    };
    item.generation = transition.generation;
    item.accepted_idempotency_keys
        .insert(transition.idempotency_key.clone());
    item.state = transition.to;
    item.superseded_by.clone_from(&transition.superseded_by);
    Ok(receipt)
}

/// Reconstruct state from an initial item and an ordered transition corpus.
pub fn replay(
    initial: &WorkItem,
    transitions: &[WorkItemTransition],
) -> Result<ReplayResult, SchemaError> {
    validate_item(initial)?;
    let mut item = initial.clone();
    let mut receipts = Vec::with_capacity(transitions.len());

    for transition in transitions {
        receipts.push(apply_transition(&mut item, transition)?);
    }

    Ok(ReplayResult { item, receipts })
}

fn validate_item(item: &WorkItem) -> Result<(), SchemaError> {
    validate_schema_version(item.schema_version)?;
    validate_key(&item.key)?;
    if item.accepted_idempotency_keys.is_empty()
        || item
            .accepted_idempotency_keys
            .iter()
            .any(|key| validate_idempotency_key(key).is_err())
    {
        return Err(SchemaError::EmptyIdempotencyKey);
    }
    let expected_generation =
        u64::try_from(item.accepted_idempotency_keys.len()).map_err(|_| {
            SchemaError::StaleGeneration {
                expected: item.generation,
                found: item.generation,
            }
        })?;
    if item.generation != expected_generation {
        return Err(SchemaError::StaleGeneration {
            expected: expected_generation,
            found: item.generation,
        });
    }
    validate_supersession_fields(&item.key, item.state, item.superseded_by.as_ref())
}

/// Validate a durable receipt, including version, terminal, and supersession invariants.
pub fn validate_receipt(receipt: &WorkItemReceipt) -> Result<(), SchemaError> {
    validate_schema_version(receipt.schema_version)?;
    validate_key(&receipt.key)?;
    validate_idempotency_key(&receipt.idempotency_key)?;
    if receipt.generation == 0 {
        return Err(SchemaError::StaleGeneration {
            expected: 1,
            found: 0,
        });
    }
    if receipt.terminal != receipt.to.is_terminal() || !legal_transition(receipt.from, receipt.to) {
        return Err(SchemaError::IllegalTransition {
            from: receipt.from,
            to: receipt.to,
        });
    }
    validate_supersession_fields(&receipt.key, receipt.to, receipt.superseded_by.as_ref())
}

fn validate_schema_version(found: u32) -> Result<(), SchemaError> {
    if found == CONTROL_PLANE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(SchemaError::UnsupportedSchemaVersion { found })
    }
}

fn validate_key(key: &WorkItemKey) -> Result<(), SchemaError> {
    let mut repository = key.repository.split('/');
    let valid_repository = repository.next().is_some_and(|part| !part.is_empty())
        && repository.next().is_some_and(|part| !part.is_empty())
        && repository.next().is_none()
        && !key.repository.chars().any(char::is_whitespace);
    let valid_sha = matches!(key.head_sha.len(), 40 | 64)
        && key
            .head_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if valid_repository && key.pull_request > 0 && valid_sha {
        Ok(())
    } else {
        Err(SchemaError::InvalidWorkItemKey)
    }
}

fn validate_idempotency_key(key: &str) -> Result<(), SchemaError> {
    if key.trim().is_empty() {
        Err(SchemaError::EmptyIdempotencyKey)
    } else {
        Ok(())
    }
}

fn validate_supersession(transition: &WorkItemTransition) -> Result<(), SchemaError> {
    validate_supersession_fields(
        &transition.key,
        transition.to,
        transition.superseded_by.as_ref(),
    )
}

fn validate_supersession_fields(
    key: &WorkItemKey,
    state: WorkItemState,
    superseded_by: Option<&WorkItemKey>,
) -> Result<(), SchemaError> {
    match (state, superseded_by) {
        (WorkItemState::Superseded, Some(replacement)) => {
            validate_key(replacement)?;
            if replacement.repository == key.repository
                && replacement.pull_request == key.pull_request
                && replacement.head_sha != key.head_sha
            {
                Ok(())
            } else {
                Err(SchemaError::InvalidSupersession)
            }
        }
        (WorkItemState::Superseded, None) => Err(SchemaError::InvalidSupersession),
        (_, Some(_)) => Err(SchemaError::UnexpectedSupersession),
        (_, None) => Ok(()),
    }
}

const fn legal_transition(from: WorkItemState, to: WorkItemState) -> bool {
    use WorkItemState::{
        Accepted, AwaitingChecks, Closed, Merged, Observing, Queued, RepairRequested, Revoked,
        Superseded,
    };
    matches!(
        (from, to),
        (
            Accepted | RepairRequested,
            Observing | Closed | Revoked | Superseded
        ) | (
            Observing,
            AwaitingChecks | RepairRequested | Closed | Revoked | Superseded
        ) | (
            AwaitingChecks,
            Queued | RepairRequested | Closed | Revoked | Superseded
        ) | (
            Queued,
            AwaitingChecks | Merged | Closed | Revoked | Superseded
        )
    )
}

#[cfg(test)]
mod tests {
    use super::{
        CONTROL_PLANE_SCHEMA_VERSION, SchemaError, WorkItem, WorkItemState, WorkItemTransition,
        replay,
    };
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Fixture {
        initial: WorkItem,
        transitions: Vec<WorkItemTransition>,
    }

    fn fixture(raw: &str) -> Fixture {
        serde_json::from_str(raw).expect("valid replay fixture")
    }

    #[test]
    fn records_round_trip_without_schema_loss() {
        let corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/head-supersession.json"
        ));
        let encoded = serde_json::to_string(&corpus.initial).expect("encode item");
        let decoded: WorkItem = serde_json::from_str(&encoded).expect("decode item");
        assert_eq!(decoded, corpus.initial);

        let encoded = serde_json::to_string(&corpus.transitions[0]).expect("encode transition");
        let decoded: WorkItemTransition =
            serde_json::from_str(&encoded).expect("decode transition");
        assert_eq!(decoded, corpus.transitions[0]);

        let replayed = replay(&corpus.initial, &corpus.transitions).expect("replay corpus");
        let receipt = &replayed.receipts[0];
        let encoded = serde_json::to_string(receipt).expect("encode receipt");
        assert_eq!(
            serde_json::from_str::<super::WorkItemReceipt>(&encoded).expect("decode receipt"),
            *receipt
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).expect("receipt json")["schema_version"],
            CONTROL_PLANE_SCHEMA_VERSION
        );
        let mut invalid_receipt = serde_json::to_value(receipt).expect("receipt value");
        invalid_receipt["terminal"] = serde_json::json!(!receipt.terminal);
        assert!(serde_json::from_value::<super::WorkItemReceipt>(invalid_receipt).is_err());
    }

    #[test]
    fn replays_head_supersession_deterministically() {
        let corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/head-supersession.json"
        ));
        let first = replay(&corpus.initial, &corpus.transitions).expect("first replay");
        let second = replay(&corpus.initial, &corpus.transitions).expect("second replay");

        assert_eq!(first, second);
        assert_eq!(first.item.generation, 4);
        assert_eq!(first.item.state, WorkItemState::Superseded);
        assert_eq!(first.item.accepted_idempotency_keys.len(), 4);
        assert_eq!(
            first.item.superseded_by,
            first.receipts.last().expect("receipt").superseded_by
        );
        assert!(first.receipts.last().expect("receipt").terminal);
        assert_ne!(
            first.receipts.last().expect("receipt").key.head_sha,
            first
                .receipts
                .last()
                .expect("receipt")
                .superseded_by
                .as_ref()
                .expect("replacement")
                .head_sha
        );
    }

    #[test]
    fn replay_rejects_duplicate_delivery() {
        let corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/duplicate-delivery.json"
        ));
        assert_eq!(
            replay(&corpus.initial, &corpus.transitions),
            Err(SchemaError::DuplicateIdempotencyKey {
                key: "incident-duplicate/observe".to_owned()
            })
        );
    }

    #[test]
    fn checkpoint_retains_full_idempotency_history() {
        let corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/head-supersession.json"
        ));
        let checkpoint = replay(&corpus.initial, &corpus.transitions[..2])
            .expect("checkpoint")
            .item;
        let mut redelivery = corpus.transitions[0].clone();
        redelivery.generation = checkpoint.generation + 1;
        redelivery.from = checkpoint.state;
        redelivery.to = WorkItemState::RepairRequested;
        assert_eq!(
            replay(&checkpoint, &[redelivery]),
            Err(SchemaError::DuplicateIdempotencyKey {
                key: "incident-head-drift/observe".to_owned()
            })
        );
    }

    #[test]
    fn replay_rejects_stale_generation() {
        let corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/stale-generation.json"
        ));
        assert_eq!(
            replay(&corpus.initial, &corpus.transitions),
            Err(SchemaError::StaleGeneration {
                expected: 2,
                found: 1
            })
        );
    }

    #[test]
    fn replay_rejects_illegal_terminal_transition() {
        let corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/illegal-terminal-transition.json"
        ));
        assert_eq!(
            replay(&corpus.initial, &corpus.transitions),
            Err(SchemaError::IllegalTransition {
                from: WorkItemState::Closed,
                to: WorkItemState::Observing
            })
        );
    }

    #[test]
    fn supersession_requires_same_pr_and_new_exact_head() {
        let mut corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/head-supersession.json"
        ));
        corpus.transitions[2]
            .superseded_by
            .as_mut()
            .expect("replacement")
            .pull_request += 1;
        assert_eq!(
            replay(&corpus.initial, &corpus.transitions),
            Err(SchemaError::InvalidSupersession)
        );
    }

    #[test]
    fn rejects_noncanonical_head_and_unknown_fields() {
        let mut corpus = fixture(include_str!(
            "../tests/fixtures/control-plane/head-supersession.json"
        ));
        corpus.initial.key.head_sha = "A111111111111111111111111111111111111111".to_owned();
        assert_eq!(
            replay(&corpus.initial, &corpus.transitions),
            Err(SchemaError::InvalidWorkItemKey)
        );

        let raw = include_str!("../tests/fixtures/control-plane/head-supersession.json");
        let with_unknown = raw.replacen(
            "\"schema_version\": 1,",
            "\"schema_version\": 1, \"future_authority\": true,",
            1,
        );
        assert!(serde_json::from_str::<Fixture>(&with_unknown).is_err());
    }
}
