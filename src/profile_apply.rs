//! Proof gates for applying a routing profile to GitHub repository variables.
//!
//! Writing a `runs-on` variable is the moment a profile stops being a document
//! and starts deciding where real jobs land. GitHub will not report a mistake
//! here: a job pointed at a label set no runner carries simply queues, forever,
//! with a green-looking workflow and no error anywhere. Worse, `runs-on` cannot
//! be changed once a job is queued, so a bad route cannot be repaired in
//! flight -- it has to be right before dispatch.
//!
//! So every lane is proved before its variable is written. The evaluation here
//! is pure: callers gather observations (runner-group membership, recent
//! dispatch evidence, topology, lease freshness) and hand them in, which keeps
//! the decision testable without a network.

use std::fmt::{Display, Formatter};

use serde::Serialize;

use crate::ci_profile::{CiProfile, HealthLease, Lane, ProfileTarget};

/// Default age limit for dispatch evidence.
pub const DEFAULT_EVIDENCE_MAX_AGE_DAYS: u32 = 7;

/// Longest a health lease may be stale and still count as live.
///
/// GitHub's queue visibility is a couple of seconds, so a lease older than
/// this risks admitting a job onto a route whose runners have since gone away.
pub const LEASE_MAX_AGE_SECONDS: i64 = 20 * 60;

/// What a caller observed about one lane's live GitHub state.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct LaneObservation {
    /// Whether the target's declared runner group exists.
    pub runner_group_found: bool,
    /// Whether that runner group grants this repository workflow access.
    pub runner_group_allows_repo: bool,
    /// Age in days of the most recent dispatch matching the evidence pattern.
    pub evidence_age_days: Option<u32>,
    /// Whether the repository's topology check passed.
    pub topology_check_passed: bool,
    /// Age in seconds of the lane's published health lease, if any.
    pub lease_age_seconds: Option<i64>,
}

/// Outcome of a single gate.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GateStatus {
    /// The gate is satisfied.
    Pass {
        /// Why it passed, in reviewable terms.
        detail: String,
    },
    /// The gate is not satisfied. The lane must not be written.
    Fail {
        /// What is missing and what would satisfy it.
        detail: String,
    },
    /// The gate does not apply to this lane.
    NotApplicable {
        /// Why it does not apply.
        detail: String,
    },
}

impl GateStatus {
    /// Whether this status blocks a write.
    #[must_use]
    pub fn blocks(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Pass { .. } => "PASS",
            Self::Fail { .. } => "FAIL",
            Self::NotApplicable { .. } => "n/a",
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Pass { detail } | Self::Fail { detail } | Self::NotApplicable { detail } => {
                detail
            }
        }
    }
}

/// One named gate and its outcome.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Gate {
    /// Gate name, stable enough to grep for.
    pub name: String,
    /// Result of evaluating it.
    pub status: GateStatus,
}

impl Display for Gate {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<24} {:<5} {}",
            self.name,
            self.status.label(),
            self.status.detail()
        )
    }
}

/// The verdict for one lane: what would be written, and whether it may be.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LaneVerdict {
    /// Context the lane belongs to.
    pub context: String,
    /// Lane name.
    pub lane: String,
    /// Target selected from the head of the fallback chain.
    pub target: Option<String>,
    /// Variable the lane publishes into.
    pub variable: Option<String>,
    /// Compact `runs-on` JSON that would be written.
    pub value: Option<String>,
    /// Every gate that was evaluated.
    pub gates: Vec<Gate>,
}

impl LaneVerdict {
    /// Whether every gate passed and there is something to write.
    #[must_use]
    pub fn writable(&self) -> bool {
        self.variable.is_some()
            && self.value.is_some()
            && !self.gates.iter().any(|gate| gate.status.blocks())
    }

    /// Gates that blocked the write.
    #[must_use]
    pub fn blocking(&self) -> Vec<&Gate> {
        self.gates
            .iter()
            .filter(|gate| gate.status.blocks())
            .collect()
    }
}

fn pass(name: &str, detail: impl Into<String>) -> Gate {
    Gate {
        name: name.to_owned(),
        status: GateStatus::Pass {
            detail: detail.into(),
        },
    }
}

fn fail(name: &str, detail: impl Into<String>) -> Gate {
    Gate {
        name: name.to_owned(),
        status: GateStatus::Fail {
            detail: detail.into(),
        },
    }
}

fn not_applicable(name: &str, detail: impl Into<String>) -> Gate {
    Gate {
        name: name.to_owned(),
        status: GateStatus::NotApplicable {
            detail: detail.into(),
        },
    }
}

/// Evaluate every gate for one lane.
///
/// `max_evidence_age_days` bounds how stale dispatch evidence may be.
#[must_use]
pub fn evaluate_lane(
    profile: &CiProfile,
    context: &str,
    lane_name: &str,
    lane: &Lane,
    observation: &LaneObservation,
    max_evidence_age_days: u32,
) -> LaneVerdict {
    let mut gates = Vec::new();

    let selected = lane
        .targets
        .first()
        .and_then(|id| profile.target(id).map(|target| (id.clone(), target)));

    let Some((target_id, target)) = selected else {
        gates.push(fail(
            "target-resolves",
            if lane.targets.is_empty() {
                "lane declares no targets".to_owned()
            } else {
                format!(
                    "lane's first target {:?} is not defined under [targets]",
                    lane.targets.first().map_or("", String::as_str)
                )
            },
        ));
        return LaneVerdict {
            context: context.to_owned(),
            lane: lane_name.to_owned(),
            target: None,
            variable: lane.github_variable.clone(),
            value: None,
            gates,
        };
    };

    gates.push(pass(
        "target-resolves",
        format!("chain head {target_id} is defined"),
    ));

    let self_managed = !target.is_github(&target_id);

    gates.push(hosted_fallback_gate(profile, lane));
    gates.push(proven_gate(&target_id, target, self_managed));
    gates.push(runner_group_gate(
        &target_id,
        target,
        observation,
        self_managed,
    ));
    gates.push(evidence_gate(
        &target_id,
        target,
        observation,
        self_managed,
        max_evidence_age_days,
    ));
    gates.push(topology_gate(observation));
    gates.push(lease_gate(lane, observation));

    LaneVerdict {
        context: context.to_owned(),
        lane: lane_name.to_owned(),
        target: Some(target_id),
        variable: lane.github_variable.clone(),
        value: target.runs_on_compact_json(),
        gates,
    }
}

/// A chain must end at GitHub-hosted runners.
///
/// `runs-on` is fixed once a job queues, so the fallback has to exist before
/// dispatch. A chain of only self-managed targets has no floor: if those hosts
/// are down, the job queues forever instead of degrading to hosted.
fn hosted_fallback_gate(profile: &CiProfile, lane: &Lane) -> Gate {
    let name = "hosted-fallback";
    match lane.targets.last() {
        None => fail(name, "lane declares no targets"),
        Some(last) => match profile.target(last) {
            Some(target) if target.is_github(last) => {
                pass(name, format!("chain terminates at hosted target {last}"))
            }
            Some(_) => fail(
                name,
                format!(
                    "chain terminates at self-managed target {last}; add a GitHub-hosted target \
                     last so a job cannot queue forever when local hosts are down"
                ),
            ),
            None => fail(
                name,
                format!("chain's last target {last} is not defined under [targets]"),
            ),
        },
    }
}

fn proven_gate(target_id: &str, target: &ProfileTarget, self_managed: bool) -> Gate {
    let name = "target-proven";
    if !self_managed {
        return not_applicable(name, format!("{target_id} is GitHub-hosted"));
    }
    if target.proven {
        pass(name, format!("{target_id} is marked proven = true"))
    } else {
        fail(
            name,
            format!(
                "{target_id} is not marked proven = true; an unproven self-managed lane is a \
                 silent black hole -- jobs queue with no error from GitHub"
            ),
        )
    }
}

fn runner_group_gate(
    target_id: &str,
    target: &ProfileTarget,
    observation: &LaneObservation,
    self_managed: bool,
) -> Gate {
    let name = "runner-group-access";
    if !self_managed {
        return not_applicable(name, format!("{target_id} is GitHub-hosted"));
    }
    let Some(group) = target.runner_group.as_deref() else {
        return fail(
            name,
            format!(
                "{target_id} declares no runner_group; the profile cannot prove the repository \
                 is allowed to use these runners"
            ),
        );
    };
    if !observation.runner_group_found {
        return fail(name, format!("runner group {group:?} was not found"));
    }
    if !observation.runner_group_allows_repo {
        return fail(
            name,
            format!("runner group {group:?} does not grant this repository workflow access"),
        );
    }
    pass(
        name,
        format!("runner group {group:?} grants this repository"),
    )
}

fn evidence_gate(
    target_id: &str,
    target: &ProfileTarget,
    observation: &LaneObservation,
    self_managed: bool,
    max_age_days: u32,
) -> Gate {
    let name = "dispatch-evidence";
    if !self_managed {
        return not_applicable(name, format!("{target_id} is GitHub-hosted"));
    }
    let Some(pattern) = target.evidence_job_pattern.as_deref() else {
        return fail(
            name,
            format!(
                "{target_id} declares no evidence_job_pattern; without one there is nothing to \
                 check a real dispatch against"
            ),
        );
    };
    match observation.evidence_age_days {
        None => fail(
            name,
            format!(
                "no dispatch matching {pattern:?} was observed on {target_id}; prove the route \
                 with a real job before pointing traffic at it"
            ),
        ),
        Some(age) if age > max_age_days => fail(
            name,
            format!(
                "most recent dispatch matching {pattern:?} is {age}d old, older than the \
                 {max_age_days}d limit"
            ),
        ),
        Some(age) => pass(
            name,
            format!("dispatch matching {pattern:?} observed {age}d ago"),
        ),
    }
}

fn topology_gate(observation: &LaneObservation) -> Gate {
    let name = "topology-check";
    if observation.topology_check_passed {
        pass(name, "runner topology check passed")
    } else {
        fail(
            name,
            "runner topology check failed; declared routes disagree with live runner state",
        )
    }
}

fn lease_gate(lane: &Lane, observation: &LaneObservation) -> Gate {
    let name = "health-lease-live";
    let lease = match HealthLease::from_fields(&lane.health_lease) {
        Ok(Some(lease)) => lease,
        Ok(None) => return not_applicable(name, "lane declares no health lease"),
        Err(error) => return fail(name, format!("health lease is invalid: {error}")),
    };
    match observation.lease_age_seconds {
        None => fail(
            name,
            format!(
                "health lease variable {} is not set; the publisher is not live, so a route \
                 written now has nothing keeping it honest",
                lease.variable
            ),
        ),
        Some(age) if age > LEASE_MAX_AGE_SECONDS => fail(
            name,
            format!(
                "health lease {} is {age}s old, past the {LEASE_MAX_AGE_SECONDS}s freshness \
                 limit; a stale lease risks queueing onto a route whose runners are gone",
                lease.variable
            ),
        ),
        Some(age) => pass(
            name,
            format!("health lease {} refreshed {age}s ago", lease.variable),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_EVIDENCE_MAX_AGE_DAYS, LaneObservation, evaluate_lane};
    use crate::ci_profile::CiProfile;

    /// A profile whose single lane passes every gate when paired with
    /// `healthy_observation`. Tests mutate one thing at a time from here.
    fn healthy_profile() -> CiProfile {
        CiProfile::parse(
            r#"
name = "local-infra"

[repo."Generous-Corp/pulp".pr.macos]
strategy = "ordered-fallback"
targets = ["macstudio.macos-arm64-vm", "github.macos-arm64"]
github_variable = "PULP_LOCAL_MACOS_RUNS_ON_JSON"

[targets."macstudio.macos-arm64-vm"]
runs_on_json = ["self-hosted", "macOS", "ARM64", "pulp-build-vm"]
proven = true
ephemeral = true
runner_group = "pulp-macos"
evidence_job_pattern = "macos"

[targets."github.macos-arm64"]
runs_on_json = "macos-15"
"#,
        )
        .expect("profile")
    }

    fn healthy_observation() -> LaneObservation {
        LaneObservation {
            runner_group_found: true,
            runner_group_allows_repo: true,
            evidence_age_days: Some(1),
            topology_check_passed: true,
            lease_age_seconds: None,
        }
    }

    fn verdict_for(profile: &CiProfile, observation: &LaneObservation) -> super::LaneVerdict {
        let lane = profile
            .lane("Generous-Corp/pulp", "pr", "macos")
            .expect("lane");
        evaluate_lane(
            profile,
            "pr",
            "macos",
            lane,
            observation,
            DEFAULT_EVIDENCE_MAX_AGE_DAYS,
        )
    }

    #[test]
    fn control_a_fully_proved_lane_is_writable() {
        // Control for every rejection test below: proves a FAIL is the one
        // thing that test mutated, not a fixture that never could pass.
        let profile = healthy_profile();

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(
            verdict.writable(),
            "healthy lane must be writable; blocking gates: {:?}",
            verdict.blocking()
        );
        assert_eq!(
            verdict.variable.as_deref(),
            Some("PULP_LOCAL_MACOS_RUNS_ON_JSON")
        );
        assert_eq!(
            verdict.value.as_deref(),
            Some(r#"["self-hosted","macOS","ARM64","pulp-build-vm"]"#)
        );
    }

    #[test]
    fn an_unproven_target_blocks_the_write() {
        let text = healthy_profile();
        let mut profile = text;
        profile
            .targets
            .get_mut("macstudio.macos-arm64-vm")
            .expect("target")
            .proven = false;

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(!verdict.writable());
        let blocking: Vec<&str> = verdict
            .blocking()
            .iter()
            .map(|gate| gate.name.as_str())
            .collect();
        assert_eq!(blocking, vec!["target-proven"]);
    }

    #[test]
    fn missing_dispatch_evidence_blocks_the_write() {
        let profile = healthy_profile();
        let observation = LaneObservation {
            evidence_age_days: None,
            ..healthy_observation()
        };

        let verdict = verdict_for(&profile, &observation);

        assert!(!verdict.writable());
        let gate = verdict
            .blocking()
            .into_iter()
            .find(|gate| gate.name == "dispatch-evidence")
            .expect("evidence gate blocks");
        assert!(
            format!("{gate}").contains("no dispatch matching"),
            "gate should explain the miss: {gate}"
        );
    }

    #[test]
    fn stale_dispatch_evidence_blocks_the_write() {
        let profile = healthy_profile();
        let observation = LaneObservation {
            evidence_age_days: Some(DEFAULT_EVIDENCE_MAX_AGE_DAYS + 1),
            ..healthy_observation()
        };

        let verdict = verdict_for(&profile, &observation);

        assert!(!verdict.writable());
        assert!(
            verdict
                .blocking()
                .iter()
                .any(|gate| gate.name == "dispatch-evidence")
        );
    }

    #[test]
    fn evidence_exactly_at_the_limit_still_passes() {
        // Boundary: the limit is inclusive, so a 7-day-old proof on a 7-day
        // limit is fine and only day 8 fails.
        let profile = healthy_profile();
        let observation = LaneObservation {
            evidence_age_days: Some(DEFAULT_EVIDENCE_MAX_AGE_DAYS),
            ..healthy_observation()
        };

        assert!(verdict_for(&profile, &observation).writable());
    }

    #[test]
    fn a_runner_group_that_excludes_the_repo_blocks_the_write() {
        let profile = healthy_profile();
        let observation = LaneObservation {
            runner_group_allows_repo: false,
            ..healthy_observation()
        };

        let verdict = verdict_for(&profile, &observation);

        assert!(!verdict.writable());
        assert!(
            verdict
                .blocking()
                .iter()
                .any(|gate| gate.name == "runner-group-access")
        );
    }

    #[test]
    fn a_target_without_a_declared_runner_group_blocks_the_write() {
        let mut profile = healthy_profile();
        profile
            .targets
            .get_mut("macstudio.macos-arm64-vm")
            .expect("target")
            .runner_group = None;

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(!verdict.writable());
        assert!(
            verdict
                .blocking()
                .iter()
                .any(|gate| gate.name == "runner-group-access")
        );
    }

    #[test]
    fn a_failing_topology_check_blocks_the_write() {
        let profile = healthy_profile();
        let observation = LaneObservation {
            topology_check_passed: false,
            ..healthy_observation()
        };

        assert!(!verdict_for(&profile, &observation).writable());
    }

    #[test]
    fn a_chain_with_no_hosted_floor_blocks_the_write() {
        // The specific danger: GitHub fixes runs-on at queue time, so a chain
        // of only self-managed targets cannot degrade to hosted in flight.
        let profile = CiProfile::parse(
            r#"
name = "local-infra"

[repo."Generous-Corp/pulp".pr.macos]
targets = ["macstudio.macos-arm64-vm"]
github_variable = "PULP_LOCAL_MACOS_RUNS_ON_JSON"

[targets."macstudio.macos-arm64-vm"]
runs_on_json = ["self-hosted", "macOS", "ARM64"]
proven = true
runner_group = "pulp-macos"
evidence_job_pattern = "macos"
"#,
        )
        .expect("profile");

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(!verdict.writable());
        let gate = verdict
            .blocking()
            .into_iter()
            .find(|gate| gate.name == "hosted-fallback")
            .expect("hosted fallback gate blocks");
        assert!(format!("{gate}").contains("queue forever"));
    }

    #[test]
    fn a_lane_without_a_lease_skips_the_lease_gate_rather_than_failing_it() {
        let profile = healthy_profile();

        let verdict = verdict_for(&profile, &healthy_observation());

        let lease_gate = verdict
            .gates
            .iter()
            .find(|gate| gate.name == "health-lease-live")
            .expect("lease gate present");
        assert!(!lease_gate.status.blocks());
        assert!(format!("{lease_gate}").contains("n/a"));
    }

    fn leased_profile() -> CiProfile {
        CiProfile::parse(
            r#"
name = "local-infra"

[repo."Generous-Corp/pulp".pr.macos]
targets = ["macstudio.macos-arm64-vm", "github.macos-arm64"]
github_variable = "PULP_LOCAL_MACOS_RUNS_ON_JSON"
health_lease_variable = "PULP_LOCAL_MACOS_LEASE_UNTIL"
health_lease_ttl_seconds = 300
health_lease_events = ["pull_request"]
health_lease_runner_name_prefix = "pulp-pr-safe-ephemeral-"
health_lease_merge_queue_branch = "main"
health_lease_admission_burst = 2
health_lease_required_capability = "pulp-pr-safe-macos-arm64"
health_lease_forbidden_capability = "pulp-auto-macos-arm64"

[targets."macstudio.macos-arm64-vm"]
runs_on_json = ["self-hosted", "macOS", "ARM64", "pulp-pr-safe-macos-arm64"]
proven = true
runner_group = "pulp-macos"
evidence_job_pattern = "macos"

[targets."github.macos-arm64"]
runs_on_json = "macos-15"
"#,
        )
        .expect("profile")
    }

    #[test]
    fn a_leased_lane_with_a_fresh_lease_is_writable() {
        let profile = leased_profile();
        let observation = LaneObservation {
            lease_age_seconds: Some(30),
            ..healthy_observation()
        };

        let verdict = verdict_for(&profile, &observation);

        assert!(verdict.writable(), "blocking: {:?}", verdict.blocking());
    }

    #[test]
    fn a_leased_lane_with_no_published_lease_blocks_the_write() {
        let profile = leased_profile();

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(!verdict.writable());
        assert!(
            verdict
                .blocking()
                .iter()
                .any(|gate| gate.name == "health-lease-live")
        );
    }

    #[test]
    fn a_stale_lease_blocks_the_write() {
        let profile = leased_profile();
        let observation = LaneObservation {
            lease_age_seconds: Some(super::LEASE_MAX_AGE_SECONDS + 1),
            ..healthy_observation()
        };

        let verdict = verdict_for(&profile, &observation);

        assert!(!verdict.writable());
        let gate = verdict
            .blocking()
            .into_iter()
            .find(|gate| gate.name == "health-lease-live")
            .expect("lease gate blocks");
        assert!(format!("{gate}").contains("freshness limit"));
    }

    #[test]
    fn a_hosted_only_lane_needs_no_self_managed_proof() {
        let profile = CiProfile::parse(
            r#"
name = "local-infra"

[repo."Generous-Corp/pulp".pr.macos]
strategy = "github-only"
targets = ["github.macos-arm64"]
github_variable = "PULP_LOCAL_MACOS_RUNS_ON_JSON"

[targets."github.macos-arm64"]
runs_on_json = "macos-15"
"#,
        )
        .expect("profile");

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(verdict.writable(), "blocking: {:?}", verdict.blocking());
        // The self-managed gates should be reported as inapplicable, not
        // silently omitted, so the dry-run output stays a full ledger.
        for name in ["target-proven", "runner-group-access", "dispatch-evidence"] {
            let gate = verdict
                .gates
                .iter()
                .find(|gate| gate.name == name)
                .unwrap_or_else(|| panic!("{name} gate should still be reported"));
            assert!(format!("{gate}").contains("n/a"), "{gate}");
        }
    }

    #[test]
    fn an_undefined_chain_head_blocks_and_reports_no_value() {
        let profile = CiProfile::parse(
            r#"
name = "local-infra"

[repo."Generous-Corp/pulp".pr.macos]
targets = ["typo.target"]
github_variable = "PULP_LOCAL_MACOS_RUNS_ON_JSON"
"#,
        )
        .expect("profile");

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(!verdict.writable());
        assert_eq!(verdict.value, None);
        assert_eq!(verdict.target, None);
    }

    #[test]
    fn a_lane_with_no_variable_is_not_writable_but_is_also_not_blocked() {
        // These are different outcomes and callers must be able to tell them
        // apart: a lane that publishes nothing is a no-op, not a failure.
        // Conflating them made the command report a problem that was not one.
        let profile = CiProfile::parse(
            r#"
name = "local-infra"

[repo."Generous-Corp/pulp".pr.macos]
strategy = "github-only"
targets = ["github.macos-arm64"]

[targets."github.macos-arm64"]
runs_on_json = "macos-15"
"#,
        )
        .expect("profile");

        let verdict = verdict_for(&profile, &healthy_observation());

        assert!(!verdict.writable(), "no variable means nothing to write");
        assert!(
            verdict.blocking().is_empty(),
            "no gate failed, so the lane is not blocked: {:?}",
            verdict.blocking()
        );
    }
}
