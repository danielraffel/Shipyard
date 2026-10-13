//! `--export-cases`: one JSONL row per case the scorecard already read, for a
//! labelled corpus built over weeks.
//!
//! A case is a red required check on a merged pull request's commit, a
//! merge-group gate that did not pass, or a merge-queue removal other than
//! `merged`. Each row carries what the scorecard read (repository, pull
//! request, head, run and job, check, outcome or cause, GitHub's time for it)
//! and, for a merged pull request, the verdict on its merge head. A row is
//! labelled by hindsight no sooner than 24 hours after it happened, so
//! `label` is always `null` here and `label_not_before` says when it may be
//! set.
//!
//! v1 reads four sources the scorecard already reads: required checks on
//! merged pull requests' commits, merge-group gate runs and their cause,
//! merge-queue removals with reason and actor, and the merge-head verdict.
//! When causes are read (see [`super::causes`]), a red case with a job also
//! carries `cause` and `evidence` from its job log. Every corpus field that
//! was not read (`evidence` when causes were not, `rule_verdict`,
//! `cancel_cause`, `receipt_same_tree`, `outage_window`) is present and
//! `null`, and named in `unread`, rather than guessed.
//!
//! The export carries a receipt: the row count and the SHA-256 of the bytes
//! written, so a copy can be checked without trusting where it came from. The
//! receipt also says where the required contexts came from; when they could
//! not be read, `required_check_red` is listed in `unread_kinds`, so its zero
//! rows read as unknown rather than as none.
//!
//! `merge_head_verdict` is `null` by kind, never by accident:
//!
//! - `required_check_red`: never `null`; the check belongs to a merged pull
//!   request, which always has a verdict.
//! - `merge_group_gate_failed`: always `null`; a merge-group run is not tied
//!   to one pull request (a batch carries several).
//! - `queue_removal`: `null` when the pull request did not merge within the
//!   window, since only merged pull requests have a merge head to judge.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{MergeVerdict, QueueEventKind, ScorecardObservation, ejection_cause, is_red, rfc3339};

/// Row schema identifier.
pub const SCHEMA: &str = "shipyard-gate-cost-case/v1";
/// Receipt schema identifier.
pub const RECEIPT_SCHEMA: &str = "shipyard-gate-cost-cases-receipt/v1";
/// How long after a case its hindsight label may be assigned.
const LABEL_DELAY_HOURS: i64 = 24;
/// Fields the plan's corpus carries that the scorecard does not read yet.
const UNREAD: [&str; 5] = [
    "evidence",
    "rule_verdict",
    "cancel_cause",
    "receipt_same_tree",
    "outage_window",
];

/// One case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Case {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// `required_check_red`, `merge_group_gate_failed` or `queue_removal`.
    pub kind: &'static str,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Pull request, when the case belongs to one.
    pub pr: Option<u64>,
    /// Commit the check ran on.
    pub head_sha: Option<String>,
    /// Workflow run.
    pub run_id: Option<u64>,
    /// Job.
    pub job_id: Option<u64>,
    /// Check name.
    pub check: Option<String>,
    /// Check outcome, gate cause, or removal reason.
    pub outcome: Option<String>,
    /// Who removed the pull request from the queue.
    pub actor: Option<String>,
    /// GitHub's time for the case.
    pub created_at: Option<String>,
    /// The merge-head verdict of the merged pull request it belongs to:
    /// always set on `required_check_red`, always `null` on
    /// `merge_group_gate_failed` (a batch is not one pull request), and
    /// `null` on `queue_removal` when the pull request did not merge within
    /// the window.
    pub merge_head_verdict: Option<MergeVerdict>,
    /// Hindsight label; assigned later, never here.
    pub label: Option<String>,
    /// Earliest time the label may be assigned.
    pub label_not_before: Option<String>,
    /// The decisive lines of the case's job log, when its cause was read
    /// (`--causes` or `--export-cases`); else `null` and named in `unread`.
    pub evidence: Option<String>,
    /// The case's `ci.cause/v1` record, read from its job log, when it was
    /// read.
    pub cause: Option<crate::diagnose::cause::CiCause>,
    /// What a rule would have decided: not read in v1.
    pub rule_verdict: Option<String>,
    /// Why a run was cancelled: not read in v1.
    pub cancel_cause: Option<String>,
    /// Whether a receipt covered the same tree: not read in v1.
    pub receipt_same_tree: Option<bool>,
    /// The outage window the case falls in: not read in v1.
    pub outage_window: Option<String>,
    /// Corpus fields not read yet; each is present and `null`.
    pub unread: Vec<&'static str>,
}

/// What an export wrote.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Receipt {
    /// [`RECEIPT_SCHEMA`].
    pub schema: &'static str,
    /// Repository.
    pub repo: String,
    /// Window start.
    pub from: String,
    /// Window end.
    pub to: String,
    /// Rows written.
    pub rows: usize,
    /// Rows by kind.
    pub by_kind: BTreeMap<&'static str, usize>,
    /// Where the required contexts came from, or `unread: <error>`.
    pub required_source: String,
    /// Kinds whose source could not be read: their row count is unknown,
    /// not zero.
    pub unread_kinds: Vec<&'static str>,
    /// SHA-256 of the JSONL bytes.
    pub sha256: String,
}

fn case(kind: &'static str, repo: &str, at: Option<DateTime<Utc>>) -> Case {
    Case {
        schema: SCHEMA,
        kind,
        repo: repo.to_owned(),
        pr: None,
        head_sha: None,
        run_id: None,
        job_id: None,
        check: None,
        outcome: None,
        actor: None,
        created_at: at.map(rfc3339),
        merge_head_verdict: None,
        label: None,
        label_not_before: at.map(|at| rfc3339(at + Duration::hours(LABEL_DELAY_HOURS))),
        evidence: None,
        cause: None,
        rule_verdict: None,
        cancel_cause: None,
        receipt_same_tree: None,
        outage_window: None,
        unread: UNREAD.to_vec(),
    }
}

/// Every case in `observation`, oldest first (cases without a time last).
#[must_use]
pub fn cases(observation: &ScorecardObservation) -> Vec<Case> {
    let repo = &observation.query.repo;
    let required: BTreeSet<&str> = observation
        .required
        .as_ref()
        .map(|(contexts, _)| contexts.iter().map(String::as_str).collect())
        .unwrap_or_default();
    let verdicts: BTreeMap<u64, MergeVerdict> = observation
        .prs
        .iter()
        .map(|pr| (pr.number, pr.verdict))
        .collect();
    let mut rows = Vec::new();
    for pr in &observation.prs {
        for commit in &pr.commits {
            for check in &commit.checks {
                let Some(outcome) = check.outcome.as_deref() else {
                    continue;
                };
                if !required.contains(check.name.as_str()) || !is_red(outcome) {
                    continue;
                }
                rows.push(Case {
                    pr: Some(pr.number),
                    head_sha: Some(commit.oid.clone()),
                    run_id: check.run_id,
                    job_id: check.job_id,
                    check: Some(check.name.clone()),
                    outcome: Some(outcome.to_owned()),
                    merge_head_verdict: Some(pr.verdict),
                    ..case("required_check_red", repo, check.at)
                });
            }
        }
    }
    for run in &observation.merge_group_runs {
        let Some(gate) = run.gate.as_ref().filter(|gate| gate.status == "completed") else {
            continue;
        };
        let Some(cause) = ejection_cause(gate) else {
            continue;
        };
        rows.push(Case {
            run_id: Some(run.run_id),
            job_id: run.gate_job_id,
            check: Some(observation.query.gate_job.clone()),
            outcome: Some(cause),
            ..case("merge_group_gate_failed", repo, Some(run.created_at))
        });
    }
    for event in &observation.queue_events {
        let QueueEventKind::Removed { reason, actor } = &event.kind else {
            continue;
        };
        if reason == "merged"
            || event.at < observation.query.from
            || event.at >= observation.query.to
        {
            continue;
        }
        rows.push(Case {
            pr: Some(event.pr),
            outcome: Some(reason.clone()),
            actor: actor.clone(),
            merge_head_verdict: verdicts.get(&event.pr).copied(),
            ..case("queue_removal", repo, Some(event.at))
        });
    }
    rows.sort_by(|left, right| {
        (
            left.created_at.is_none(),
            &left.created_at,
            left.kind,
            left.pr,
            left.run_id,
        )
            .cmp(&(
                right.created_at.is_none(),
                &right.created_at,
                right.kind,
                right.pr,
                right.run_id,
            ))
    });
    rows
}

/// The JSONL bytes of `rows` and their receipt.
///
/// # Errors
///
/// When a row cannot be serialized.
pub fn export(
    observation: &ScorecardObservation,
    rows: &[Case],
) -> Result<(Vec<u8>, Receipt), String> {
    let mut bytes = Vec::new();
    let mut by_kind: BTreeMap<&'static str, usize> = BTreeMap::new();
    for row in rows {
        serde_json::to_writer(&mut bytes, row).map_err(|error| error.to_string())?;
        bytes.push(b'\n');
        *by_kind.entry(row.kind).or_insert(0) += 1;
    }
    let unread_kinds = if observation.required.is_err() {
        vec!["required_check_red"]
    } else {
        Vec::new()
    };
    let receipt = Receipt {
        schema: RECEIPT_SCHEMA,
        repo: observation.query.repo.clone(),
        from: rfc3339(observation.query.from),
        to: rfc3339(observation.query.to),
        rows: rows.len(),
        by_kind,
        required_source: super::required_source(observation),
        unread_kinds,
        sha256: hex::encode(Sha256::digest(&bytes)),
    };
    Ok((bytes, receipt))
}
