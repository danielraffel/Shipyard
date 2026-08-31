//! Generic, read-only classification for queued GitHub jobs that remain
//! unassigned despite compatible idle self-hosted capacity.
//!
//! Classification never mutates GitHub, the merge queue, or runners. A
//! separate publisher may project one exact, stable `DISPATCH_WEDGE` into the
//! existing native continuation ledger; that transaction is idempotent and
//! therefore survives publisher restart without duplicating a wake.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Authoritative identity for one required merge-group job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DispatchJobAuthority {
    pub repository: String,
    pub base_ref: String,
    pub pull_request: u64,
    pub pull_request_head: String,
    pub queue_position: u64,
    pub merge_group_head: String,
    pub workflow_run_id: u64,
    pub workflow_id: u64,
    pub run_attempt: u64,
    pub run_event: String,
    pub run_head: String,
    pub job_id: u64,
    pub job_name: String,
    pub job_status: String,
    pub job_conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub labels: Vec<String>,
    pub queued_at: String,
    pub required_context: String,
    pub required_app_id: Option<u64>,
    pub producer_app_id: Option<u64>,
}

/// One repository-visible runner. This represents present GitHub scheduler
/// capacity only; local admission holds govern future JIT/VM creation and are
/// intentionally outside this read-only classifier.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DispatchRunnerObservation {
    pub runner_id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
    pub labels: Vec<String>,
}

/// One daemon observer result. The observer owns only GitHub/fleet facts; the
/// single actionable-wake producer owns temporal stability and publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DispatchWedgeObservation {
    pub(crate) authority: DispatchJobAuthority,
    pub(crate) runners: Vec<DispatchRunnerObservation>,
    pub(crate) observation_complete: bool,
}

/// One complete joined observation.
pub struct DispatchWedgeInputs<'a> {
    pub authority: &'a DispatchJobAuthority,
    pub runners: &'a [DispatchRunnerObservation],
    pub observation_complete: bool,
    /// Digest returned by `dispatch_wedge_observation_digest` for the prior
    /// complete read. A missing or different digest remains waiting.
    pub previous_observation_digest: Option<&'a str>,
    pub assignment_threshold_secs: i64,
    pub now: DateTime<Utc>,
}

/// Stable classifier state for automation and human-readable projections.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchWedgeState {
    NotApplicable,
    Waiting,
    NoCompatibleCapacity,
    Indeterminate,
    DispatchWedge,
}

/// Exact evidence permitted to wake the existing logical owner.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DispatchWedgeEvidence {
    pub dedupe_key: String,
    pub evidence_digest: String,
    pub repository: String,
    pub base_ref: String,
    pub pull_request: u64,
    pub pull_request_head: String,
    pub queue_position: u64,
    pub merge_group_head: String,
    pub workflow_run_id: u64,
    pub workflow_id: u64,
    pub run_attempt: u64,
    pub run_event: String,
    pub run_head: String,
    pub job_id: u64,
    pub job_name: String,
    pub job_status: String,
    pub job_conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub required_context: String,
    pub required_app_id: Option<u64>,
    pub producer_app_id: Option<u64>,
    pub queued_at: String,
    pub assignment_age_secs: i64,
    pub labels: Vec<String>,
    pub required_labels_digest: String,
    pub eligible_idle_runners: Vec<DispatchRunnerEvidence>,
    pub observation_digest: String,
    pub observed_at: String,
}

/// Exact compatible-idle and clear-hold evidence that authorized a wedge.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct DispatchRunnerEvidence {
    pub runner_id: u64,
    pub name: String,
    pub status: String,
    pub busy: bool,
    pub labels: Vec<String>,
    pub capacity_basis: DispatchCapacityBasis,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchCapacityBasis {
    GitHubRegisteredOnlineIdle,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DispatchWedgeAssessment {
    pub state: DispatchWedgeState,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<DispatchWedgeEvidence>,
}

/// Classify one exact joined observation without changing external state.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one fail-closed decision tree keeps every non-wedge exit auditable"
)]
pub fn assess_dispatch_wedge(inputs: &DispatchWedgeInputs<'_>) -> DispatchWedgeAssessment {
    let authority = inputs.authority;
    let indeterminate = |reason: &str| DispatchWedgeAssessment {
        state: DispatchWedgeState::Indeterminate,
        reason: reason.to_owned(),
        evidence: None,
    };
    if !inputs.observation_complete {
        return indeterminate("observation_incomplete");
    }
    if validate_authority(authority).is_err() {
        return indeterminate("authority_invalid");
    }
    let current_observation_digest = dispatch_wedge_observation_digest(authority, inputs.runners);
    if authority.run_event != "merge_group"
        || !authority
            .run_head
            .eq_ignore_ascii_case(&authority.merge_group_head)
        || !authority.job_status.eq_ignore_ascii_case("queued")
        || authority.job_conclusion.is_some()
        || authority
            .runner_name
            .as_deref()
            .is_some_and(|name| !name.is_empty())
        || !authority
            .required_context
            .eq_ignore_ascii_case(&authority.job_name)
    {
        return DispatchWedgeAssessment {
            state: DispatchWedgeState::NotApplicable,
            reason: "job_not_exact_queued_required_merge_group".to_owned(),
            evidence: None,
        };
    }
    if authority
        .required_app_id
        .is_some_and(|required| authority.producer_app_id != Some(required))
    {
        return indeterminate("required_check_app_provenance_mismatch");
    }
    let labels = normalized_labels(&authority.labels);
    if labels.is_empty() || !labels.contains("self-hosted") {
        return indeterminate("job_labels_not_self_hosted");
    }
    let queued_at = match DateTime::parse_from_rfc3339(&authority.queued_at) {
        Ok(value) => value.with_timezone(&Utc),
        Err(_) => return indeterminate("queued_at_invalid"),
    };
    if queued_at > inputs.now {
        return indeterminate("queued_at_in_future");
    }
    if inputs.assignment_threshold_secs <= 0 {
        return indeterminate("assignment_threshold_invalid");
    }
    let age = (inputs.now - queued_at).num_seconds();
    if age < inputs.assignment_threshold_secs {
        return DispatchWedgeAssessment {
            state: DispatchWedgeState::Waiting,
            reason: "assignment_threshold_not_reached".to_owned(),
            evidence: None,
        };
    }
    if inputs.previous_observation_digest != Some(current_observation_digest.as_str()) {
        return DispatchWedgeAssessment {
            state: DispatchWedgeState::Waiting,
            reason: "matching_second_read_required".to_owned(),
            evidence: None,
        };
    }

    let compatible = inputs
        .runners
        .iter()
        .filter(|runner| runner.status.eq_ignore_ascii_case("online") && !runner.busy)
        .filter(|runner| labels.is_subset(&normalized_labels(&runner.labels)))
        .collect::<Vec<_>>();
    if compatible.is_empty() {
        return DispatchWedgeAssessment {
            state: DispatchWedgeState::NoCompatibleCapacity,
            reason: "no_compatible_idle_runner".to_owned(),
            evidence: None,
        };
    }
    if compatible
        .iter()
        .any(|runner| runner.runner_id == 0 || runner.name.trim().is_empty())
    {
        return indeterminate("compatible_runner_identity_invalid");
    }
    let compatible_names = compatible
        .iter()
        .map(|runner| runner.name.to_ascii_lowercase())
        .collect::<BTreeSet<_>>();
    if compatible_names.len() != compatible.len() {
        return indeterminate("compatible_runner_identity_duplicated");
    }
    let mut eligible_idle_runners = compatible
        .iter()
        .map(|runner| DispatchRunnerEvidence {
            runner_id: runner.runner_id,
            name: runner.name.clone(),
            status: runner.status.to_ascii_lowercase(),
            busy: runner.busy,
            labels: normalized_labels(&runner.labels).into_iter().collect(),
            capacity_basis: DispatchCapacityBasis::GitHubRegisteredOnlineIdle,
        })
        .collect::<Vec<_>>();
    eligible_idle_runners.sort();
    let observed_at = inputs.now.to_rfc3339();
    let mut evidence = DispatchWedgeEvidence {
        dedupe_key: String::new(),
        evidence_digest: String::new(),
        repository: authority.repository.to_ascii_lowercase(),
        base_ref: authority.base_ref.clone(),
        pull_request: authority.pull_request,
        pull_request_head: authority.pull_request_head.to_ascii_lowercase(),
        queue_position: authority.queue_position,
        merge_group_head: authority.merge_group_head.to_ascii_lowercase(),
        workflow_run_id: authority.workflow_run_id,
        workflow_id: authority.workflow_id,
        run_attempt: authority.run_attempt,
        run_event: authority.run_event.clone(),
        run_head: authority.run_head.to_ascii_lowercase(),
        job_id: authority.job_id,
        job_name: authority.job_name.clone(),
        job_status: authority.job_status.clone(),
        job_conclusion: authority.job_conclusion.clone(),
        runner_name: authority.runner_name.clone(),
        required_context: authority.required_context.clone(),
        required_app_id: authority.required_app_id,
        producer_app_id: authority.producer_app_id,
        queued_at: authority.queued_at.clone(),
        assignment_age_secs: age,
        labels: labels.iter().cloned().collect(),
        required_labels_digest: required_labels_digest(&labels),
        eligible_idle_runners,
        observation_digest: current_observation_digest,
        observed_at,
    };
    evidence.dedupe_key = transition_dedupe_key(&evidence);
    evidence.evidence_digest = evidence_digest(&evidence);
    DispatchWedgeAssessment {
        state: DispatchWedgeState::DispatchWedge,
        reason: "queued_unassigned_with_compatible_idle_capacity".to_owned(),
        evidence: Some(evidence),
    }
}

fn validate_authority(authority: &DispatchJobAuthority) -> Result<(), ()> {
    let canonical_repo = authority.repository == authority.repository.trim()
        && authority.repository.split('/').count() == 2
        && authority.repository.split('/').all(|part| !part.is_empty());
    if !canonical_repo
        || authority.base_ref.trim().is_empty()
        || authority.pull_request == 0
        || authority.workflow_run_id == 0
        || authority.workflow_id == 0
        || authority.run_attempt == 0
        || authority.job_id == 0
        || !is_full_sha(&authority.pull_request_head)
        || !is_full_sha(&authority.merge_group_head)
        || !is_full_sha(&authority.run_head)
    {
        return Err(());
    }
    Ok(())
}

fn is_full_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn normalized_labels(labels: &[String]) -> BTreeSet<String> {
    labels
        .iter()
        .map(|label| label.trim().to_ascii_lowercase())
        .filter(|label| !label.is_empty())
        .collect()
}

fn required_labels_digest(labels: &BTreeSet<String>) -> String {
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(labels).expect("serializable labels"))
    )
}

#[derive(Serialize)]
struct CanonicalObservationAuthority<'a> {
    repository: String,
    base_ref: &'a str,
    pull_request: u64,
    pull_request_head: String,
    queue_position: u64,
    merge_group_head: String,
    workflow_run_id: u64,
    workflow_id: u64,
    run_attempt: u64,
    run_event: String,
    run_head: String,
    job_id: u64,
    job_name: &'a str,
    job_status: String,
    job_conclusion: Option<String>,
    runner_name: Option<String>,
    labels: Vec<String>,
    queued_at: &'a str,
    required_context: &'a str,
    required_app_id: Option<u64>,
    producer_app_id: Option<u64>,
}

#[derive(Eq, Ord, PartialEq, PartialOrd, Serialize)]
struct CanonicalRunnerObservation {
    runner_id: u64,
    name: String,
    status: String,
    busy: bool,
    labels: Vec<String>,
}

/// Digest one complete authority/registered-capacity read so a later classification
/// can prove it observed the same snapshot twice.
#[must_use]
pub(crate) fn dispatch_wedge_observation_digest(
    authority: &DispatchJobAuthority,
    runners: &[DispatchRunnerObservation],
) -> String {
    let canonical_authority = CanonicalObservationAuthority {
        repository: authority.repository.to_ascii_lowercase(),
        base_ref: &authority.base_ref,
        pull_request: authority.pull_request,
        pull_request_head: authority.pull_request_head.to_ascii_lowercase(),
        queue_position: authority.queue_position,
        merge_group_head: authority.merge_group_head.to_ascii_lowercase(),
        workflow_run_id: authority.workflow_run_id,
        workflow_id: authority.workflow_id,
        run_attempt: authority.run_attempt,
        run_event: authority.run_event.to_ascii_lowercase(),
        run_head: authority.run_head.to_ascii_lowercase(),
        job_id: authority.job_id,
        job_name: &authority.job_name,
        job_status: authority.job_status.to_ascii_lowercase(),
        job_conclusion: authority
            .job_conclusion
            .as_ref()
            .map(|value| value.to_ascii_lowercase()),
        runner_name: authority
            .runner_name
            .as_ref()
            .map(|value| value.to_ascii_lowercase()),
        labels: normalized_labels(&authority.labels).into_iter().collect(),
        queued_at: &authority.queued_at,
        required_context: &authority.required_context,
        required_app_id: authority.required_app_id,
        producer_app_id: authority.producer_app_id,
    };
    let required_labels = normalized_labels(&authority.labels);
    let mut canonical_runners = runners
        .iter()
        // Unrelated ephemeral runners must not reset the two-read gate. Keep
        // only capacity whose labels could satisfy this exact job; status,
        // busy and HOLD changes for those runners remain digest-bound.
        .filter(|runner| required_labels.is_subset(&normalized_labels(&runner.labels)))
        .map(|runner| CanonicalRunnerObservation {
            runner_id: runner.runner_id,
            name: runner.name.to_ascii_lowercase(),
            status: runner.status.to_ascii_lowercase(),
            busy: runner.busy,
            labels: normalized_labels(&runner.labels).into_iter().collect(),
        })
        .collect::<Vec<_>>();
    canonical_runners.sort();
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(canonical_authority, canonical_runners))
                .expect("serializable observation")
        )
    )
}

fn evidence_digest(evidence: &DispatchWedgeEvidence) -> String {
    let mut canonical = evidence.clone();
    canonical.dedupe_key.clear();
    canonical.evidence_digest.clear();
    canonical.observed_at.clear();
    canonical.assignment_age_secs = 0;
    format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&canonical).expect("serializable evidence"))
    )
}

fn transition_dedupe_key(evidence: &DispatchWedgeEvidence) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(
                evidence.repository.to_ascii_lowercase(),
                evidence.base_ref.as_str(),
                evidence.pull_request,
                evidence.pull_request_head.to_ascii_lowercase(),
                evidence.merge_group_head.to_ascii_lowercase(),
                evidence.workflow_run_id,
                evidence.workflow_id,
                evidence.run_attempt,
                evidence.job_id,
            ))
            .expect("serializable transition identity")
        )
    )
}

/// Result of projecting one exact wedge into the existing native wake ledger.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(crate) struct DispatchWedgeWakeReceipt {
    pub(crate) dedupe_key: String,
    pub(crate) matched: bool,
    pub(crate) changed: bool,
    pub(crate) wake_enqueued: bool,
    pub(crate) ledger_phase: Option<String>,
}

/// Publish a classified wedge through the existing transactional lifecycle
/// event and outbox. Non-wedge observations remain read-only.
pub(crate) fn publish_dispatch_wedge(
    ledger: &crate::work_ledger::WorkLedger,
    repository_provider: Option<&str>,
    repository_id: Option<&str>,
    assessment: &DispatchWedgeAssessment,
) -> Result<Option<DispatchWedgeWakeReceipt>, String> {
    if assessment.state != DispatchWedgeState::DispatchWedge {
        return Ok(None);
    }
    let evidence = assessment
        .evidence
        .as_ref()
        .ok_or_else(|| "dispatch-wedge assessment omitted exact evidence".to_owned())?;
    if evidence.dedupe_key != transition_dedupe_key(evidence)
        || evidence.evidence_digest != evidence_digest(evidence)
        || evidence.required_labels_digest
            != required_labels_digest(&evidence.labels.iter().cloned().collect())
    {
        return Err("dispatch-wedge evidence identity or digest mismatch".to_owned());
    }
    let report = ledger
        .publish_dispatch_wedge(
            repository_provider,
            repository_id,
            &evidence.repository,
            &evidence.base_ref,
            evidence.pull_request,
            &evidence.pull_request_head,
            &evidence.dedupe_key,
            &evidence.evidence_digest,
        )
        .map_err(|error| error.to_string())?;
    Ok(Some(DispatchWedgeWakeReceipt {
        dedupe_key: evidence.dedupe_key.clone(),
        matched: report.matched,
        changed: report.changed,
        wake_enqueued: report.wake_enqueued,
        ledger_phase: report.phase,
    }))
}

#[cfg(test)]
mod tests;
