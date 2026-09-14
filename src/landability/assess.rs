//! Resolving required contexts to lanes, and lanes to verdicts.
//!
//! This is the algorithm the 2026-09-13 outage needed and nobody had:
//!
//! ```text
//! for context in required_contexts:
//!     producers = jobs whose rendered name (or id) equals context
//!     for job in transitive_needs(producers) ∪ producers:
//!         assess(resolve_runs_on(job))
//! ```
//!
//! The `transitive_needs` line is the whole difference between a check that
//! fires and a check that does not. On the fleet this was written against, the
//! required `macos` context's own job was routed through a variable that was
//! fine; the two preamble jobs it *needs* were routed through the one that was
//! not. A check that looked only at the context's producer would have returned
//! a clean bill of health during the outage.

use chrono::{DateTime, Utc};

use super::workflow::{RunsOnResolution, WorkflowJob, resolve_runs_on_expr, transitive_needs};
use super::{
    AttestationSet, LandabilityReport, LaneAssessment, LaneRole, Schedulability, assess_lane,
};
use crate::fleet_service::{Boundary, LaneServiceThresholds, RegisteredRunner};

/// Inputs to one landability assessment. Every field is data the caller
/// gathered; nothing here performs I/O, so the whole classifier is testable
/// against captured fixtures.
pub struct AssessInput<'a> {
    /// Required contexts to resolve.
    pub contexts: &'a [String],
    /// Where the contexts came from, for the report.
    pub contexts_source: &'a str,
    /// Jobs parsed out of the workflow files.
    pub jobs: &'a [WorkflowJob],
    /// Routing variables by name.
    pub variables: &'a [(String, String)],
    /// Runner census spanning both scopes.
    pub census: &'a [RegisteredRunner],
    /// Boundary that stopped the census, if any.
    pub census_boundary: Option<Boundary>,
    /// Boundary that stopped the routing-variable read, if any.
    pub variables_boundary: Option<Boundary>,
    /// Host attestations.
    pub attestations: &'a AttestationSet,
    /// Thresholds for the underlying service assertion.
    pub thresholds: LaneServiceThresholds,
    /// Labels the operator has explicitly waived.
    pub allow_unserved: &'a [String],
}

/// Resolve every required context to its lanes and assess each one.
#[must_use]
pub fn assess(input: &AssessInput<'_>, now: DateTime<Utc>) -> LandabilityReport {
    let mut report = LandabilityReport {
        contexts: input.contexts.to_vec(),
        contexts_source: input.contexts_source.to_owned(),
        lanes: Vec::new(),
        contexts_without_producer: Vec::new(),
        fresh_attesters: input.attestations.fresh_hosts(now),
        warnings: Vec::new(),
    };

    if report.fresh_attesters.is_empty() {
        report.warnings.push(format!(
            "no fresh host attestation: {} - lanes with an empty census are reported Unknown, \
             not Unserved, because a dead sensor must not be able to block every ship",
            input.attestations.describe_staleness(now)
        ));
    }

    for context in input.contexts {
        let producers: Vec<String> = input
            .jobs
            .iter()
            .filter(|job| job.produces(context))
            .map(|job| job.id.clone())
            .collect();
        if producers.is_empty() {
            // Not a fault by itself: a required context may come from a
            // workflow this preflight did not read (a separate file, or an
            // external App). Reported so the gap is visible rather than
            // mistaken for a pass.
            report.contexts_without_producer.push(context.clone());
            continue;
        }
        let closure = transitive_needs(input.jobs, &producers);
        for job_id in closure {
            let Some(job) = input.jobs.iter().find(|job| job.id == job_id) else {
                continue;
            };
            let role = if producers.contains(&job_id) {
                LaneRole::Producer
            } else {
                LaneRole::Prerequisite
            };
            report
                .lanes
                .extend(assess_job(input, context, job, role, now));
        }
    }

    // Honour operator waivers last, so the report still records what was
    // found and only the *action* changes.
    for lane in &mut report.lanes {
        if lane.verdict != Schedulability::Unserved {
            continue;
        }
        let waived = lane
            .report
            .declaration
            .labels()
            .iter()
            .any(|label| input.allow_unserved.iter().any(|allow| allow == label));
        if waived {
            lane.verdict = Schedulability::Unknown;
            lane.attestation_faults
                .push("WAIVED by --allow-unserved-lane; this is a validation gap".to_owned());
        }
    }

    report
}

fn assess_job(
    input: &AssessInput<'_>,
    context: &str,
    job: &WorkflowJob,
    role: LaneRole,
    now: DateTime<Utc>,
) -> Vec<LaneAssessment> {
    let Some(expr) = job.runs_on_expr.as_deref() else {
        return vec![unknown_lane(
            context,
            &job.id,
            role,
            "runs-on absent",
            Boundary::Parse,
            now,
            input,
        )];
    };

    match resolve_runs_on_expr(expr) {
        RunsOnResolution::Literal { value } => vec![assess_lane(
            context,
            &job.id,
            role,
            "literal",
            &value,
            input.census,
            input.census_boundary,
            input.attestations,
            input.thresholds,
            now,
        )],
        RunsOnResolution::Variable { name, fallback } => {
            // An unread variable and an unset one are indistinguishable from
            // here, and only one of them is safe to assume. Falling back to
            // the workflow's hosted literal when the variables call FAILED
            // reports `Served` for a lane that was never measured — observed
            // live on a host whose App token was returning 404, which produced
            // five confident `served` verdicts out of nothing.
            if let Some(boundary) = input.variables_boundary {
                return vec![unknown_lane(
                    context,
                    &job.id,
                    role,
                    &format!(
                        "vars.{name} could not be read, so this lane was not measured; the \
                         workflow's own fallback is NOT assumed"
                    ),
                    boundary,
                    now,
                    input,
                )];
            }
            let raw = input
                .variables
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
                // An unset variable, on the other hand, IS a hole the workflow
                // fills itself: its own literal is what GitHub will use, and
                // treating that as unknown would alarm on every correctly
                // defaulted job.
                .or(fallback);
            match raw {
                Some(raw) => vec![assess_lane(
                    context,
                    &job.id,
                    role,
                    &name,
                    &raw,
                    input.census,
                    input.census_boundary,
                    input.attestations,
                    input.thresholds,
                    now,
                )],
                None => vec![unknown_lane(
                    context,
                    &job.id,
                    role,
                    &format!("vars.{name} unset and the expression supplies no fallback"),
                    Boundary::Parse,
                    now,
                    input,
                )],
            }
        }
        RunsOnResolution::Dynamic { origin, from_jobs } => {
            assess_dynamic_lane(input, context, job, role, &origin, &from_jobs, now)
        }
        RunsOnResolution::Unparsable { raw } => vec![unknown_lane(
            context,
            &job.id,
            role,
            &format!("runs-on expression not understood: {raw}"),
            Boundary::Parse,
            now,
            input,
        )],
    }
}

/// Assess a `runs-on` computed at run time from a job output or a matrix.
///
/// The candidate set is every `vars.*_RUNS_ON_JSON` the producing jobs read.
/// The lane is schedulable if **any** candidate is, because exactly one will
/// be selected at run time and a static analysis cannot know which — an
/// over-approximation in the permissive direction, which is the correct
/// direction for a gate whose false positive is a refused ship.
#[allow(clippy::too_many_arguments)]
fn assess_dynamic_lane(
    input: &AssessInput<'_>,
    context: &str,
    job: &WorkflowJob,
    role: LaneRole,
    origin: &str,
    from_jobs: &[String],
    now: DateTime<Utc>,
) -> Vec<LaneAssessment> {
    let mut sources: Vec<String> = from_jobs.to_vec();
    if sources.is_empty() {
        sources.clone_from(&job.needs);
    }
    let mut candidates: Vec<String> = Vec::new();
    for source in &sources {
        if let Some(producer) = input.jobs.iter().find(|candidate| candidate.id == *source) {
            for name in &producer.var_refs {
                if name.ends_with("_RUNS_ON_JSON") && !candidates.contains(name) {
                    candidates.push(name.clone());
                }
            }
        }
    }
    if candidates.is_empty() {
        return vec![unknown_lane(
            context,
            &job.id,
            role,
            &format!(
                "runs-on comes from {origin} and no *_RUNS_ON_JSON candidate is statically \
                 visible in {}",
                if sources.is_empty() {
                    "its needs".to_owned()
                } else {
                    sources.join(",")
                }
            ),
            Boundary::Parse,
            now,
            input,
        )];
    }
    let all = candidates.join(",");
    let assessments: Vec<LaneAssessment> = candidates
        .iter()
        .filter_map(|name| {
            let raw = input
                .variables
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())?;
            Some(assess_lane(
                context,
                &job.id,
                role,
                &format!("{name} (via {origin}; candidates {all})"),
                &raw,
                input.census,
                input.census_boundary,
                input.attestations,
                input.thresholds,
                now,
            ))
        })
        .collect();
    if assessments
        .iter()
        .any(|lane| !lane.verdict.blocks() && lane.verdict != Schedulability::Unknown)
    {
        let mut best = assessments;
        best.sort_by_key(|lane| lane.verdict);
        best.truncate(1);
        return best;
    }
    assessments
}

fn unknown_lane(
    context: &str,
    job_id: &str,
    role: LaneRole,
    detail: &str,
    boundary: Boundary,
    now: DateTime<Utc>,
    input: &AssessInput<'_>,
) -> LaneAssessment {
    // Routed through `assess_lane` with a deliberately unparsable value so the
    // Unknown carries a boundary and a next action, exactly like every other
    // Unknown in the system. An unknown that cannot say why is the failure
    // this shape exists to prevent.
    let mut lane = assess_lane(
        context,
        job_id,
        role,
        "unresolved",
        "",
        input.census,
        Some(boundary),
        input.attestations,
        input.thresholds,
        now,
    );
    lane.verdict = Schedulability::Unknown;
    lane.report.detail = format!("{detail} - {}", boundary.next_action());
    lane
}
