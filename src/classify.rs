//! Coarse failure classification.
//!
//! Executors classify failed target results so retry and failover logic
//! can distinguish infrastructure failures from authoritative test
//! failures without parsing raw logs at every call site.

use serde::Serialize;

/// Stable failure taxonomy.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FailureClass {
    /// Network, SSH, runner, or provider availability problem.
    Infra,
    /// Executor wall-clock budget expired.
    Timeout,
    /// Declared validation contract was not satisfied.
    Contract,
    /// Non-zero validation failure with no infra marker.
    Test,
    /// Working tree changed during a local `shipyard run`.
    TreeDrift,
    /// Fallback for ambiguous failures.
    Unknown,
}

impl FailureClass {
    /// Return the Python-compatible string value.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Infra => "INFRA",
            Self::Timeout => "TIMEOUT",
            Self::Contract => "CONTRACT",
            Self::Test => "TEST",
            Self::TreeDrift => "TREE_DRIFT",
            Self::Unknown => "UNKNOWN",
        }
    }

    /// Parse the uppercase label produced by [`FailureClass::as_str`] back into a
    /// class. Case-sensitive on purpose — the emitted labels are always uppercase,
    /// and a loose parse could silently accept a foreign producer's lowercase
    /// value that means something else. Unknown/empty → `None`.
    #[must_use]
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "INFRA" => Some(Self::Infra),
            "TIMEOUT" => Some(Self::Timeout),
            "CONTRACT" => Some(Self::Contract),
            "TEST" => Some(Self::Test),
            "TREE_DRIFT" => Some(Self::TreeDrift),
            "UNKNOWN" => Some(Self::Unknown),
            _ => None,
        }
    }
}

impl std::fmt::Display for FailureClass {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

const INFRA_MARKERS: [&str; 12] = [
    "Connection refused",
    "ssh: connect",
    "Network is unreachable",
    "Could not resolve host",
    "RUN_IN_DAYS_DEAD",
    "github runner offline",
    "No route to host",
    "kex_exchange_identification",
    "Connection reset by peer",
    "Connection closed by remote host",
    "Connection timed out",
    "ssh_exchange_identification",
];

/// Classify a non-successful target outcome.
#[must_use]
pub fn classify_failure(
    _stdout: &str,
    stderr: &str,
    exit_code: i32,
    wall_clock_exceeded: bool,
    contract_violated: bool,
) -> FailureClass {
    if contract_violated {
        return FailureClass::Contract;
    }
    if wall_clock_exceeded {
        return FailureClass::Timeout;
    }
    if INFRA_MARKERS.iter().any(|marker| stderr.contains(marker)) {
        return FailureClass::Infra;
    }
    if exit_code != 0 {
        return FailureClass::Test;
    }
    FailureClass::Unknown
}

/// Return whether the failure class is worth retrying once. This is the broad
/// taxonomy predicate (`INFRA` or `TIMEOUT`) intended for cross-backend failover
/// policies. Same-backend local retry is deliberately stricter — see
/// [`same_leg_local_retryable`].
#[must_use]
pub fn is_retryable(failure_class: FailureClass) -> bool {
    matches!(failure_class, FailureClass::Infra | FailureClass::Timeout)
}

/// Whether a failed LOCAL leg is worth re-running once on the SAME backend.
///
/// Deliberately narrower than [`is_retryable`]: only a transient `INFRA` blip
/// qualifies. A local `TIMEOUT` failure means the leg already burned its full
/// (large) wall-clock budget on a host that is likely still slow, so re-running
/// it in place would merely double the wait and almost certainly time out again.
/// Every other class is authoritative. A missing or unparseable label → not
/// retryable (fail toward the honest single attempt).
#[must_use]
pub fn same_leg_local_retryable(failure_class: Option<&str>) -> bool {
    matches!(
        failure_class.and_then(FailureClass::from_label),
        Some(FailureClass::Infra)
    )
}

/// Return the class a failed target's `failure_class` should become when — and
/// ONLY when — the caller has independently confirmed a host infrastructure
/// incident (jetsam / `WindowServer` crash) overlapped the leg. `Some(Infra)` if
/// `current` is `TEST`, else `None`.
///
/// This function does NOT check for an incident itself: it is a pure
/// eligibility rule. Only a `TEST` failure (a non-zero exit with no infra
/// marker) is promotable — exactly the ambiguous case a concurrent host incident
/// best explains. `CONTRACT`, `TIMEOUT`, `TREE_DRIFT`, an already-`INFRA`, and
/// `UNKNOWN` are authoritative and kept, so a genuine validation-contract
/// violation is never masked behind an infra label. Callers must gate on a
/// confirmed overlap (see `crate::host_health::incident_from_path`) before
/// applying the result.
#[must_use]
pub fn promote_test_to_infra(current: Option<&str>) -> Option<FailureClass> {
    if current == Some(FailureClass::Test.as_str()) {
        Some(FailureClass::Infra)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{FailureClass, classify_failure, is_retryable};

    #[test]
    fn contract_violation_takes_priority() {
        assert_eq!(
            classify_failure("", "Connection refused", 0, true, true,),
            FailureClass::Contract
        );
    }

    #[test]
    fn timeout_takes_priority_over_infra_markers() {
        assert_eq!(
            classify_failure("", "Connection refused", 255, true, false),
            FailureClass::Timeout
        );
    }

    #[test]
    fn infra_markers_classify_as_infra() {
        for marker in [
            "Connection refused",
            "ssh: connect",
            "Network is unreachable",
            "Could not resolve host",
            "RUN_IN_DAYS_DEAD",
            "github runner offline",
            "No route to host",
            "kex_exchange_identification",
            "Connection reset by peer",
            "Connection closed by remote host",
            "Connection timed out",
            "ssh_exchange_identification",
        ] {
            assert_eq!(
                classify_failure("", marker, 255, false, false),
                FailureClass::Infra,
                "{marker}"
            );
        }
    }

    #[test]
    fn nonzero_without_markers_is_test_failure() {
        assert_eq!(
            classify_failure("", "assertion failed", 1, false, false),
            FailureClass::Test
        );
    }

    #[test]
    fn zero_without_flags_is_unknown() {
        assert_eq!(
            classify_failure("", "", 0, false, false),
            FailureClass::Unknown
        );
    }

    #[test]
    fn from_label_round_trips_every_class() {
        for class in [
            FailureClass::Infra,
            FailureClass::Timeout,
            FailureClass::Contract,
            FailureClass::Test,
            FailureClass::TreeDrift,
            FailureClass::Unknown,
        ] {
            assert_eq!(FailureClass::from_label(class.as_str()), Some(class));
        }
    }

    #[test]
    fn from_label_rejects_unknown_and_lowercase() {
        assert_eq!(FailureClass::from_label(""), None);
        assert_eq!(FailureClass::from_label("infra"), None);
        assert_eq!(FailureClass::from_label("timeout"), None);
        assert_eq!(FailureClass::from_label("NOT_A_CLASS"), None);
    }

    #[test]
    fn same_leg_local_retry_is_infra_only() {
        use super::same_leg_local_retryable;
        assert!(same_leg_local_retryable(Some("INFRA")));
        // Stricter than the global taxonomy: a local TIMEOUT is NOT re-run.
        assert!(!same_leg_local_retryable(Some("TIMEOUT")));
        assert!(!same_leg_local_retryable(Some("CONTRACT")));
        assert!(!same_leg_local_retryable(Some("TEST")));
        assert!(!same_leg_local_retryable(Some("TREE_DRIFT")));
        assert!(!same_leg_local_retryable(Some("UNKNOWN")));
        // Unparseable / missing label fails toward a single honest attempt.
        assert!(!same_leg_local_retryable(Some("infra")));
        assert!(!same_leg_local_retryable(None));
    }

    #[test]
    fn retryable_only_for_infra_and_timeout() {
        assert!(is_retryable(FailureClass::Infra));
        assert!(is_retryable(FailureClass::Timeout));
        assert!(!is_retryable(FailureClass::Contract));
        assert!(!is_retryable(FailureClass::Test));
        assert!(!is_retryable(FailureClass::TreeDrift));
        assert!(!is_retryable(FailureClass::Unknown));
    }

    #[test]
    fn serializes_python_string_values() {
        assert_eq!(
            serde_json::to_string(&FailureClass::Infra).expect("json"),
            r#""INFRA""#
        );
    }

    #[test]
    fn promote_only_acts_on_test() {
        use super::promote_test_to_infra;
        assert_eq!(
            promote_test_to_infra(Some("TEST")),
            Some(FailureClass::Infra)
        );
    }

    #[test]
    fn promote_keeps_authoritative_classes() {
        use super::promote_test_to_infra;
        // A real contract/timeout/tree-drift/infra/unknown is never masked.
        for class in ["CONTRACT", "TIMEOUT", "TREE_DRIFT", "INFRA", "UNKNOWN"] {
            assert_eq!(
                promote_test_to_infra(Some(class)),
                None,
                "{class} must not be reclassified"
            );
        }
        assert_eq!(promote_test_to_infra(None), None);
    }
}
