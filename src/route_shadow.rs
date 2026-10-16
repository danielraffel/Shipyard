//! Shadow-only route and capability observations.
//!
//! This module consumes an immutable observation of the routes that an
//! authoritative scheduler would already consider. It does not probe hosts,
//! acquire leases, reorder candidates, or dispatch work. The configured
//! candidate order therefore remains the authority; the receipt only records
//! what each later candidate would have done if earlier candidates were not
//! available.

use serde::{Deserialize, Serialize};

/// Schema version for [`RouteCapabilitySnapshot`] and [`RouteShadowReceipt`].
pub const ROUTE_SHADOW_SCHEMA_VERSION: u32 = 1;

/// One immutable route/capability observation supplied to the shadow pass.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteCapabilitySnapshot {
    /// Schema version of this snapshot.
    pub schema_version: u32,
    /// Stable request identity supplied by the queue owner.
    pub request_id: String,
    /// Logical target being evaluated.
    pub target_name: String,
    /// Capabilities required by the target artifact.
    pub required_capabilities: Vec<String>,
    /// Candidates in authoritative configured order. This vector is never
    /// sorted by the shadow evaluator.
    pub candidates: Vec<RouteCandidateSnapshot>,
}

/// One route candidate observed without performing a probe or dispatch.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteCandidateSnapshot {
    /// Stable route/member identity.
    pub route_id: String,
    /// Environment where execution would occur (for example `macos-arm64` or
    /// `github-hosted-macos`).
    pub execution_environment: String,
    /// Artifact capability the environment claims to provide. Keeping this
    /// separate from the execution environment leaves room for cross-build
    /// hosts without changing the route schema.
    pub artifact_capability: String,
    /// Capabilities observed for this candidate.
    pub capabilities: Vec<String>,
    /// Admission state already observed by the authoritative scheduler.
    pub availability: RouteAvailability,
}

/// Admission state captured in a route snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteAvailability {
    /// Candidate can accept the request now.
    Available,
    /// Candidate is configured but currently occupied.
    Busy,
    /// Candidate could not be reached by the authoritative observer.
    Unreachable,
    /// The observer could not establish a trustworthy state.
    Unknown,
}

/// Why a route was or was not admitted in the shadow comparison.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteCounterfactualOutcome {
    /// This route is the first available candidate in configured order.
    Authoritative,
    /// This route would be selected if all earlier candidates were unavailable.
    WouldAdmitIfEarlierUnavailable,
    /// The route lacks one or more required capabilities.
    CapabilityMismatch {
        /// Required capabilities absent from this route.
        missing: Vec<String>,
    },
    /// The route has the capabilities but is not currently admissible.
    Unavailable {
        /// Observed admission state.
        availability: RouteAvailability,
    },
}

/// Typed observation of the authoritative route and its counterfactuals.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteShadowReceipt {
    /// Schema version of this receipt.
    pub schema_version: u32,
    /// Request identity copied from the snapshot.
    pub request_id: String,
    /// Logical target copied from the snapshot.
    pub target_name: String,
    /// Canonical required capability set (sorted and deduplicated).
    pub required_capabilities: Vec<String>,
    /// Selected route, if any candidate was admissible.
    pub authoritative_route: Option<RouteSelection>,
    /// One typed outcome for every candidate, preserving configured order.
    pub counterfactuals: Vec<RouteCounterfactual>,
    /// Explicit proof that this receipt did not mutate dispatch state.
    pub control: RouteShadowControl,
}

/// Selected route identity and its position in the ordered snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteSelection {
    /// Candidate route identity.
    pub route_id: String,
    /// Zero-based position in the authoritative candidate order.
    pub position: usize,
}

/// Per-candidate counterfactual outcome.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteCounterfactual {
    /// Candidate route identity.
    pub route_id: String,
    /// Zero-based position in the authoritative candidate order.
    pub position: usize,
    /// Execution environment copied from the snapshot.
    pub execution_environment: String,
    /// Artifact capability copied from the snapshot.
    pub artifact_capability: String,
    /// Typed comparison outcome.
    pub outcome: RouteCounterfactualOutcome,
}

/// Controls and provenance for a shadow receipt.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RouteShadowControl {
    /// Authority whose ordering was evaluated.
    pub authority: String,
    /// Whether this pass changed dispatch state. Always false in this module.
    pub dispatch_mutated: bool,
    /// Whether a worker or external route was invoked. Always false here.
    pub dispatch_performed: bool,
}

/// Errors raised when a snapshot cannot be treated as authoritative input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RouteShadowError {
    /// Snapshot schema is not understood.
    UnsupportedSchema(u32),
    /// Required request identity is absent.
    EmptyRequestId,
    /// Required target identity is absent.
    EmptyTargetName,
    /// At least one ordered candidate is required.
    EmptyCandidates,
    /// Candidate identity is absent.
    EmptyRouteId {
        /// Position of the invalid candidate.
        position: usize,
    },
    /// Candidate identities must be unique to keep receipts unambiguous.
    DuplicateRouteId(String),
}

impl std::fmt::Display for RouteShadowError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported route shadow schema {version}")
            }
            Self::EmptyRequestId => {
                formatter.write_str("route shadow request_id must not be empty")
            }
            Self::EmptyTargetName => {
                formatter.write_str("route shadow target_name must not be empty")
            }
            Self::EmptyCandidates => {
                formatter.write_str("route shadow candidates must not be empty")
            }
            Self::EmptyRouteId { position } => write!(
                formatter,
                "route shadow candidate {position} has an empty route_id"
            ),
            Self::DuplicateRouteId(route_id) => {
                write!(formatter, "route shadow route_id is duplicated: {route_id}")
            }
        }
    }
}

impl std::error::Error for RouteShadowError {}

/// Evaluate an immutable route snapshot without touching dispatch state.
///
/// The first candidate that is both available and capable is authoritative.
/// Later capable candidates are recorded as counterfactuals, which gives a
/// benchmark/control pair without allowing shadow observations to reroute a
/// real job.
pub fn evaluate(
    snapshot: &RouteCapabilitySnapshot,
) -> Result<RouteShadowReceipt, RouteShadowError> {
    validate_snapshot(snapshot)?;
    let required = canonical_capabilities(&snapshot.required_capabilities);
    let authoritative_position = snapshot
        .candidates
        .iter()
        .enumerate()
        .find(|(_, candidate)| {
            candidate.availability == RouteAvailability::Available
                && missing_capabilities(candidate, &required).is_empty()
        })
        .map(|(position, _)| position);

    let counterfactuals = snapshot
        .candidates
        .iter()
        .enumerate()
        .map(|(position, candidate)| {
            let missing = missing_capabilities(candidate, &required);
            let outcome = if !missing.is_empty() {
                RouteCounterfactualOutcome::CapabilityMismatch { missing }
            } else if candidate.availability != RouteAvailability::Available {
                RouteCounterfactualOutcome::Unavailable {
                    availability: candidate.availability,
                }
            } else if Some(position) == authoritative_position {
                RouteCounterfactualOutcome::Authoritative
            } else {
                RouteCounterfactualOutcome::WouldAdmitIfEarlierUnavailable
            };
            RouteCounterfactual {
                route_id: candidate.route_id.clone(),
                position,
                execution_environment: candidate.execution_environment.clone(),
                artifact_capability: candidate.artifact_capability.clone(),
                outcome,
            }
        })
        .collect();

    Ok(RouteShadowReceipt {
        schema_version: ROUTE_SHADOW_SCHEMA_VERSION,
        request_id: snapshot.request_id.clone(),
        target_name: snapshot.target_name.clone(),
        required_capabilities: required,
        authoritative_route: authoritative_position.map(|position| RouteSelection {
            route_id: snapshot.candidates[position].route_id.clone(),
            position,
        }),
        counterfactuals,
        control: RouteShadowControl {
            authority: "ordered_route_selection".to_owned(),
            dispatch_mutated: false,
            dispatch_performed: false,
        },
    })
}

fn validate_snapshot(snapshot: &RouteCapabilitySnapshot) -> Result<(), RouteShadowError> {
    if snapshot.schema_version != ROUTE_SHADOW_SCHEMA_VERSION {
        return Err(RouteShadowError::UnsupportedSchema(snapshot.schema_version));
    }
    if snapshot.request_id.trim().is_empty() {
        return Err(RouteShadowError::EmptyRequestId);
    }
    if snapshot.target_name.trim().is_empty() {
        return Err(RouteShadowError::EmptyTargetName);
    }
    if snapshot.candidates.is_empty() {
        return Err(RouteShadowError::EmptyCandidates);
    }
    let mut seen = std::collections::BTreeSet::new();
    for (position, candidate) in snapshot.candidates.iter().enumerate() {
        if candidate.route_id.trim().is_empty() {
            return Err(RouteShadowError::EmptyRouteId { position });
        }
        if !seen.insert(candidate.route_id.as_str()) {
            return Err(RouteShadowError::DuplicateRouteId(
                candidate.route_id.clone(),
            ));
        }
    }
    Ok(())
}

fn canonical_capabilities(capabilities: &[String]) -> Vec<String> {
    let mut canonical = capabilities
        .iter()
        .map(|capability| capability.trim().to_owned())
        .filter(|capability| !capability.is_empty())
        .collect::<Vec<_>>();
    canonical.sort();
    canonical.dedup();
    canonical
}

fn missing_capabilities(candidate: &RouteCandidateSnapshot, required: &[String]) -> Vec<String> {
    let capabilities = candidate
        .capabilities
        .iter()
        .map(|capability| capability.trim())
        .collect::<std::collections::BTreeSet<_>>();
    required
        .iter()
        .filter(|required| !capabilities.contains(required.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        ROUTE_SHADOW_SCHEMA_VERSION, RouteAvailability, RouteCandidateSnapshot,
        RouteCapabilitySnapshot, RouteCounterfactualOutcome, RouteShadowError, evaluate,
    };

    fn candidate(
        route_id: &str,
        environment: &str,
        artifact: &str,
        capabilities: &[&str],
        availability: RouteAvailability,
    ) -> RouteCandidateSnapshot {
        RouteCandidateSnapshot {
            route_id: route_id.to_owned(),
            execution_environment: environment.to_owned(),
            artifact_capability: artifact.to_owned(),
            capabilities: capabilities
                .iter()
                .map(|capability| (*capability).to_owned())
                .collect(),
            availability,
        }
    }

    fn snapshot(candidates: Vec<RouteCandidateSnapshot>) -> RouteCapabilitySnapshot {
        RouteCapabilitySnapshot {
            schema_version: ROUTE_SHADOW_SCHEMA_VERSION,
            request_id: "job-1".to_owned(),
            target_name: "mac".to_owned(),
            required_capabilities: vec!["arm64".to_owned(), "macos".to_owned()],
            candidates,
        }
    }

    #[test]
    fn preserves_ordered_authority_and_records_capable_fallback() {
        let receipt = evaluate(&snapshot(vec![
            candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
            candidate(
                "overflow",
                "github-hosted-macos",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
        ]))
        .expect("receipt");

        assert_eq!(
            receipt
                .authoritative_route
                .as_ref()
                .map(|route| route.route_id.as_str()),
            Some("primary")
        );
        assert!(matches!(
            receipt.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::Authoritative
        ));
        assert!(matches!(
            receipt.counterfactuals[1].outcome,
            RouteCounterfactualOutcome::WouldAdmitIfEarlierUnavailable
        ));
        assert!(!receipt.control.dispatch_mutated);
        assert!(!receipt.control.dispatch_performed);
    }

    #[test]
    fn control_selects_overflow_only_when_primary_is_unavailable() {
        let receipt = evaluate(&snapshot(vec![
            candidate(
                "primary",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Busy,
            ),
            candidate(
                "overflow",
                "github-hosted-macos",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
        ]))
        .expect("receipt");

        assert_eq!(
            receipt
                .authoritative_route
                .as_ref()
                .map(|route| route.route_id.as_str()),
            Some("overflow")
        );
        assert!(matches!(
            receipt.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::Unavailable {
                availability: RouteAvailability::Busy
            }
        ));
        assert!(matches!(
            receipt.counterfactuals[1].outcome,
            RouteCounterfactualOutcome::Authoritative
        ));
    }

    #[test]
    fn capability_admission_is_independent_from_execution_environment() {
        let receipt = evaluate(&snapshot(vec![candidate(
            "cross-build",
            "linux-arm64",
            "macos-arm64",
            &["macos", "arm64"],
            RouteAvailability::Available,
        )]))
        .expect("receipt");

        assert_eq!(
            receipt
                .authoritative_route
                .as_ref()
                .map(|route| route.route_id.as_str()),
            Some("cross-build")
        );
        assert_eq!(
            receipt.counterfactuals[0].execution_environment,
            "linux-arm64"
        );
        assert_eq!(
            receipt.counterfactuals[0].artifact_capability,
            "macos-arm64"
        );
    }

    #[test]
    fn unavailable_and_missing_capabilities_are_typed() {
        let receipt = evaluate(&snapshot(vec![
            candidate(
                "wrong-cap",
                "linux",
                "linux",
                &["linux"],
                RouteAvailability::Available,
            ),
            candidate(
                "busy",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Busy,
            ),
        ]))
        .expect("receipt");

        assert!(matches!(receipt.authoritative_route, None));
        assert!(matches!(
            receipt.counterfactuals[0].outcome,
            RouteCounterfactualOutcome::CapabilityMismatch { .. }
        ));
        assert!(matches!(
            receipt.counterfactuals[1].outcome,
            RouteCounterfactualOutcome::Unavailable {
                availability: RouteAvailability::Busy
            }
        ));
    }

    #[test]
    fn rejects_ambiguous_snapshots() {
        let mut invalid = snapshot(vec![
            candidate(
                "same",
                "macos-arm64",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
            candidate(
                "same",
                "github-hosted-macos",
                "macos-arm64",
                &["macos", "arm64"],
                RouteAvailability::Available,
            ),
        ]);
        assert!(matches!(
            evaluate(&invalid),
            Err(RouteShadowError::DuplicateRouteId(_))
        ));
        invalid.schema_version = 99;
        assert!(matches!(
            evaluate(&invalid),
            Err(RouteShadowError::UnsupportedSchema(99))
        ));
    }
}
