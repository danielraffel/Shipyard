//! Same-base keyed-reuse canary orchestration.
//!
//! The canary is deliberately stricter than ordinary shadow comparison. The
//! keyed pass may consume only the record published by the full pass in the
//! same invocation, with the same base snapshot, tree, and lane configuration.
//! A missing or mismatched full-pass record is an inconclusive observation.

use serde::{Deserialize, Serialize};

/// The immutable identity published by the full-shadow pass.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FullShadowRecord {
    /// The pinned protected-base commit.
    pub base_sha: String,
    /// The tree tested by both passes.
    pub tree_sha: String,
    /// Digest of the lane configuration and validation contract.
    pub lane_config_sha256: String,
    /// Host-local reuse record identifier published by pass 1.
    pub record_id: String,
}

/// The pass-2 binding decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KeyedPassBinding {
    /// Pass 2 is bound to the exact pass-1 record.
    Bound(FullShadowRecord),
    /// The comparison cannot be judged from this invocation.
    Inconclusive {
        /// Bounded reason the same-base comparison cannot be judged.
        reason: String,
    },
}

/// Missing selected return-code evidence is inconclusive, never equal.
#[must_use]
pub fn selected_returncode_matches(lane_rc: i32, selected_rc: Option<i32>) -> Option<bool> {
    selected_rc.map(|selected| selected == lane_rc)
}

/// Bind pass 2 to pass 1. No record from an earlier invocation or a different
/// base/configuration is eligible.
#[must_use]
pub fn bind_keyed_pass(
    pass1: Option<&FullShadowRecord>,
    candidate: Option<&FullShadowRecord>,
) -> KeyedPassBinding {
    let Some(pass1) = pass1 else {
        return KeyedPassBinding::Inconclusive {
            reason: "missing_pass1_record".to_owned(),
        };
    };
    let Some(candidate) = candidate else {
        return KeyedPassBinding::Inconclusive {
            reason: "missing_pass2_binding".to_owned(),
        };
    };
    if candidate != pass1 {
        return KeyedPassBinding::Inconclusive {
            reason: "pass2_record_does_not_match_pass1_identity".to_owned(),
        };
    }
    KeyedPassBinding::Bound(pass1.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> FullShadowRecord {
        FullShadowRecord {
            base_sha: "a".repeat(40),
            tree_sha: "b".repeat(40),
            lane_config_sha256: "c".repeat(64),
            record_id: "full-run-1".to_owned(),
        }
    }
    #[test]
    fn pass2_binds_only_to_the_exact_pass1_record() {
        let first = record();
        assert_eq!(
            bind_keyed_pass(Some(&first), Some(&first)),
            KeyedPassBinding::Bound(first)
        );
    }
    #[test]
    fn a_different_record_is_refused_even_when_the_base_matches() {
        let first = record();
        let mut other = first.clone();
        other.record_id = "full-run-older".to_owned();
        assert_eq!(
            bind_keyed_pass(Some(&first), Some(&other)),
            KeyedPassBinding::Inconclusive {
                reason: "pass2_record_does_not_match_pass1_identity".to_owned()
            }
        );
    }
    #[test]
    fn missing_pass1_is_inconclusive_and_never_a_pass() {
        assert_eq!(
            bind_keyed_pass(None, Some(&record())),
            KeyedPassBinding::Inconclusive {
                reason: "missing_pass1_record".to_owned()
            }
        );
    }
    #[test]
    fn a_changed_base_or_lane_configuration_is_refused() {
        let first = record();
        for mutate in [
            |r: &mut FullShadowRecord| r.base_sha.replace_range(..1, "d"),
            |r: &mut FullShadowRecord| r.lane_config_sha256.replace_range(..1, "e"),
        ] {
            let mut other = first.clone();
            mutate(&mut other);
            assert!(matches!(
                bind_keyed_pass(Some(&first), Some(&other)),
                KeyedPassBinding::Inconclusive { .. }
            ));
        }
    }
    #[test]
    fn absent_selected_returncode_is_inconclusive() {
        assert_eq!(selected_returncode_matches(0, None), None);
        assert_eq!(selected_returncode_matches(0, Some(0)), Some(true));
        assert_eq!(selected_returncode_matches(0, Some(1)), Some(false));
    }
}
