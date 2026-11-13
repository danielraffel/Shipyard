//! Re-run a required check that failed for an infrastructure reason, on any
//! open pull request, so a mirror outage or a lost runner does not leave a
//! head red until a person notices.
//!
//! The classification is `shipyard diagnose`'s own ([`crate::diagnose::classify`]):
//! a required job is re-run only when the diagnosis calls it `infra` (a lost
//! runner, a non-content step such as checkout or `Install … dependencies`,
//! a starved sibling's fail-closed hold, or a transport/5xx marker in a step
//! no test named). Everything else is left alone:
//!
//! - the approval context (`review-verdict`): its red means "awaiting a
//!   reviewer", and its failing step is a non-content one, so the classifier
//!   alone would call it infra. It is excluded by name before any read, and a
//!   run that contains a job of that name is never re-run either, since
//!   `rerun-failed-jobs` re-runs failed jobs and their dependents;
//! - a run in which any job, required or not, failed for a reason that is not
//!   a diagnosed infra failure of a required job: `rerun-failed-jobs` would
//!   re-run that real failure too;
//! - cancelled and timed-out required checks, which belong to the steward's
//!   interruption path (`RerunTransient`), so the two never act on one check.
//!
//! Budget, per (head SHA, check): one re-run; then, only after
//! [`BACKOFF_SECS`] and only if the re-run also failed as infra, a second;
//! then none. The cap is also fenced by GitHub's own `run_attempt`, so a lost
//! ledger cannot buy a third, and the re-run intent is saved before the POST.
//! When a check is capped, or when [`OUTAGE_MIN_PRS`] open pull requests are
//! still red (or re-running) on the same check after an infra call at the same
//! step, the repository report carries one finding, "infra outage suspected:
//! <check> failing on N PRs at step '<step>'", which the operator pass prints,
//! so an outage is reported rather than retried forever.
//!
//! Reads: the census gives each managed head's check rollup; heads the census
//! did not hydrate (unmanaged pull requests) are read in batched rollup
//! queries of six heads, at most [`MAX_HYDRATED_HEADS`] per pass. A head with
//! nothing red costs nothing more. A red candidate whose run is still in
//! flight in the rollup costs nothing either. Otherwise it costs one jobs
//! read, its failing required jobs' logs and any annotations a cancel
//! classification needs, once per red observation: the ledger remembers the
//! check id it judged. At most [`MAX_RUN_DIAGNOSES`] runs are diagnosed per
//! repository pass, newest first, inside the pass budget.
//!
//! Known blind spot, accepted because a wrong call costs one run: a head can
//! make a "non-content" step fail itself (a bad submodule pin breaks
//! checkout; a changed lockfile breaks `Install … dependencies`). Such a red
//! is re-run at most twice and then reported.
//!
//! A re-run never approves, arms or changes code.
//!
//! Prior art: Chromium CQ retries a build that failed for an infra reason
//! with a per-CL retry quota; Buildkite and GitHub retry a job automatically
//! when its agent or runner is lost; Bazel's `--flaky_test_attempts` and
//! Tricium bound retries per target. All bound the retry and report the
//! residue, as this does.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Instant;

use serde::{Deserialize, Serialize};

use super::{GitHubActions, ObservedPr, RepoObservation, RequiredCheck, StewardLedger};
use crate::diagnose::{CheckDiagnosis, Diagnosis, Job, classify};
use crate::merge_steward::{
    OWNER_DECISION_LABEL, PARKED_LABEL_PREFIX, SUPERSEDED_LABEL, StewardCheck, StewardCheckSource,
    is_full_sha,
};

/// Re-runs per (head SHA, check), the first included.
pub(super) const MAX_RERUNS: u32 = 2;
/// Minimum wait after the first re-run before a second, and after a refused
/// re-run or an unreadable diagnosis before trying again.
pub(super) const BACKOFF_SECS: i64 = 20 * 60;
/// Refused re-runs or unreadable diagnoses of one red before it is settled.
pub(super) const MAX_ERRORS: u32 = 2;
/// Open pull requests red on one check at one step that make an outage
/// finding even before any of them is capped.
pub(super) const OUTAGE_MIN_PRS: usize = 3;
/// An outage finding is remembered this long after it was last seen, so a
/// brief gap does not report it as new again.
pub(super) const OUTAGE_GRACE_SECS: i64 = 2 * BACKOFF_SECS;
/// Workflow runs diagnosed per repository pass.
pub(super) const MAX_RUN_DIAGNOSES: usize = 8;
/// Unhydrated heads whose rollup the sweep reads per repository pass.
pub(super) const MAX_HYDRATED_HEADS: usize = 60;
/// The approval context is excluded whatever `--approval-context` names.
const APPROVAL_CONTEXT: &str = "review-verdict";
/// Reported step of a job that recorded none (a lost runner).
const NO_STEP: &str = "(no step)";

/// What the steward does with an infra red.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Mode {
    /// Nothing; no reads.
    Off,
    /// Diagnose and report what it would re-run; re-run nothing.
    Shadow,
    /// Re-run within the caps. The default: a 7-day fleet back-test
    /// (2026-11-04..11) found 22 of 23 verifiable infra calls passed on
    /// re-run or failed again as infra at the same step (0.957).
    #[default]
    Enforce,
}

impl Mode {
    /// Parse `off`, `shadow` or `enforce`.
    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "enforce" => Ok(Self::Enforce),
            other => Err(format!(
                "--infra-rerun must be off, shadow or enforce, not `{other}`"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }
}

/// What the ledger keeps per (repo, PR, head, check).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct Record {
    /// Pull request.
    #[serde(default)]
    pub(super) pr: u64,
    /// Check name.
    #[serde(default)]
    pub(super) check: String,
    /// Re-runs GitHub accepted (shadow never counts one).
    #[serde(default)]
    pub(super) reruns: u32,
    /// When the last re-run was issued (epoch seconds).
    #[serde(default)]
    pub(super) last_rerun_epoch: Option<i64>,
    /// The check id (Actions job id) of the red last judged.
    #[serde(default)]
    pub(super) judged_check_id: Option<u64>,
    /// `rerun`, `would_rerun` (shadow), `declined`, `capped`, `refused`
    /// (settled after [`MAX_ERRORS`] errors), or `error` (a refused re-run or
    /// an unreadable diagnosis, tried again after the backoff).
    #[serde(default)]
    pub(super) verdict: String,
    /// The diagnosis rule behind the verdict, or the decline reason.
    #[serde(default)]
    pub(super) why: String,
    /// Failing step of the red last judged.
    #[serde(default)]
    pub(super) step: String,
    /// Failing test names the diagnosis of the red last judged named (sorted),
    /// for red-everywhere grouping ([`super::red_everywhere`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) tests: Vec<String>,
    /// Errors on the red last judged.
    #[serde(default)]
    pub(super) errors: u32,
    /// When the last error happened.
    #[serde(default)]
    pub(super) last_error_epoch: Option<i64>,
}

/// One outage finding's lifetime in the ledger.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) struct OutageSeen {
    pub(super) first_epoch: i64,
    pub(super) last_seen_epoch: i64,
}

/// Ledger key for one check on one exact head.
pub(super) fn record_key(repo: &str, pr: u64, head: &str, check: &str) -> String {
    format!(
        "{}#{pr}@{}:{check}",
        repo.to_ascii_lowercase(),
        head.to_ascii_lowercase()
    )
}

fn repo_prefix(repo: &str) -> String {
    format!("{}#", repo.to_ascii_lowercase())
}

/// What one red check needs this pass, from its ledger record alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Next {
    /// Read and classify it.
    Diagnose,
    /// Wait out a backoff.
    Wait { remaining_secs: i64 },
    /// This exact red was already re-run (or, in shadow, would have been);
    /// nothing changes until a new attempt shows.
    Pending,
    /// Already judged; nothing more to do (`declined`, `capped`, `refused`).
    Settled(String),
}

fn wait_until(at: Option<i64>, now: i64) -> Option<Next> {
    let ready = at? + BACKOFF_SECS;
    (now < ready).then_some(Next::Wait {
        remaining_secs: ready - now,
    })
}

/// What to do with a red check whose current check id is `check_id`.
pub(super) fn next(record: Option<&Record>, check_id: u64, now_epoch: i64, mode: Mode) -> Next {
    let Some(record) = record else {
        return Next::Diagnose;
    };
    if record.judged_check_id == Some(check_id) {
        return match record.verdict.as_str() {
            // Shadow's call is not a re-run: an enforcing pass judges it.
            "would_rerun" if mode == Mode::Enforce => Next::Diagnose,
            "rerun" | "would_rerun" => Next::Pending,
            "error" if record.errors >= MAX_ERRORS => Next::Settled("refused".to_owned()),
            "error" => wait_until(record.last_error_epoch, now_epoch).unwrap_or(Next::Diagnose),
            other => Next::Settled(other.to_owned()),
        };
    }
    // A newer red than the one last judged.
    if record.verdict == "capped" {
        return Next::Settled("capped".to_owned());
    }
    if record.reruns > 0
        && let Some(wait) = wait_until(record.last_rerun_epoch, now_epoch)
    {
        return wait;
    }
    // Includes a spent budget: the red is classified once, then capped (infra)
    // or declined (anything else), so a real failure is never called an outage.
    Next::Diagnose
}

/// Whether `name` is the approval context, under either spelling.
pub(super) fn is_approval_context(name: &str, approval_context: &str) -> bool {
    let name = name.trim();
    name.eq_ignore_ascii_case(APPROVAL_CONTEXT)
        || (!approval_context.trim().is_empty()
            && name.eq_ignore_ascii_case(approval_context.trim()))
}

/// One red required check on a current head, before any read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub(super) pr: u64,
    pub(super) head_sha: String,
    pub(super) check: String,
    pub(super) run_id: u64,
    pub(super) check_id: u64,
    /// Another check of the same run on this head has not completed.
    pub(super) run_in_flight: bool,
}

fn required(check: &StewardCheck, required: &[RequiredCheck]) -> bool {
    required.iter().any(|rule| {
        rule.context == check.name && rule.app_id.is_none_or(|app| check.app_id == Some(app))
    })
}

/// The newest observation of each check name on a head.
fn latest_checks(checks: &[StewardCheck]) -> Vec<&StewardCheck> {
    let mut by_name: BTreeMap<&str, &StewardCheck> = BTreeMap::new();
    for check in checks {
        match by_name.get(check.name.as_str()) {
            Some(seen) if seen.check_id.unwrap_or(0) >= check.check_id.unwrap_or(0) => {}
            _ => {
                by_name.insert(check.name.as_str(), check);
            }
        }
    }
    by_name.into_values().collect()
}

fn is(value: Option<&str>, expected: &str) -> bool {
    value.is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

/// Every failed required check on each open pull request's current head:
/// the newest observation of each name, completed `FAILURE`, from a check run
/// or a commit status, with or without Actions ids, not the approval context,
/// on a pull request not excluded. Unlike [`candidates`] it keeps checks no
/// re-run could act on, because red-everywhere grouping
/// ([`super::red_everywhere`]) needs every red. Reads nothing.
pub(super) fn failed_required<'a>(
    observation: &'a RepoObservation,
    approval_context: &str,
    excluded_labels: &[String],
) -> Vec<(&'a ObservedPr, &'a StewardCheck)> {
    let mut out = Vec::new();
    for pr in &observation.prs {
        if pr_excluded(pr, excluded_labels) {
            continue;
        }
        for check in latest_checks(&pr.fact.checks) {
            if is(Some(&check.status), "completed")
                && is(check.conclusion.as_deref(), "failure")
                && !is_approval_context(&check.name, approval_context)
                && required(check, &observation.required_checks)
            {
                out.push((pr, check));
            }
        }
    }
    out
}

/// Every failed required Actions check on a current head that may be infra:
/// completed `FAILURE`, with a run and a job id, not the approval context,
/// on a pull request not excluded. `checks` holds each pull request's
/// current-head checks. Reads nothing.
pub(super) fn candidates(
    observation: &RepoObservation,
    checks: &BTreeMap<u64, Vec<StewardCheck>>,
    approval_context: &str,
    excluded_labels: &[String],
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for pr in &observation.prs {
        if pr_excluded(pr, excluded_labels) {
            continue;
        }
        let Some(head_checks) = checks.get(&pr.fact.number) else {
            continue;
        };
        for check in latest_checks(head_checks) {
            if check.source != StewardCheckSource::CheckRun
                || !is(Some(&check.status), "completed")
                || !is(check.conclusion.as_deref(), "failure")
                || is_approval_context(&check.name, approval_context)
                || !required(check, &observation.required_checks)
            {
                continue;
            }
            let (Some(run_id), Some(check_id)) = (check.run_id, check.check_id) else {
                continue;
            };
            let run_in_flight = head_checks
                .iter()
                .any(|other| other.run_id == Some(run_id) && !is(Some(&other.status), "completed"));
            out.push(Candidate {
                pr: pr.fact.number,
                head_sha: pr.fact.head_sha.clone(),
                check: check.name.clone(),
                run_id,
                check_id,
                run_in_flight,
            });
        }
    }
    out
}

fn pr_excluded(pr: &ObservedPr, excluded_labels: &[String]) -> bool {
    pr.fact.labels.iter().any(|label| {
        label.eq_ignore_ascii_case(SUPERSEDED_LABEL)
            || label.eq_ignore_ascii_case(OWNER_DECISION_LABEL)
            || label.to_ascii_lowercase().starts_with(PARKED_LABEL_PREFIX)
            || excluded_labels
                .iter()
                .any(|excluded| label.eq_ignore_ascii_case(excluded))
    })
}

/// Pull requests whose checks the census did not read and whose checks can
/// still matter: open, not draft, not queued, not conflicting.
fn unhydrated_heads(observation: &RepoObservation, excluded_labels: &[String]) -> Vec<String> {
    observation
        .prs
        .iter()
        .filter(|pr| {
            pr.fact.checks.is_empty()
                && !pr.fact.draft
                && pr.fact.queue_position.is_none()
                && is_full_sha(&pr.fact.head_sha)
                && !matches!(
                    pr.fact.merge_state.to_ascii_uppercase().as_str(),
                    "DIRTY" | "CONFLICTING"
                )
                && !pr_excluded(pr, excluded_labels)
        })
        .map(|pr| pr.fact.head_sha.clone())
        .take(MAX_HYDRATED_HEADS)
        .collect()
}

/// The verdict on one diagnosed run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RunVerdict {
    /// Every failure in the run is a diagnosed infra failure of a required job.
    Rerun,
    /// Not now: the run still has jobs in flight.
    InFlight,
    /// Infra again, but GitHub already re-ran this run [`MAX_RERUNS`] times.
    Capped,
    /// Not infra, or not provably so.
    Decline(String),
}

/// Judge one run from its diagnosis and jobs. `head_sha` is the PR's current
/// head, and `candidates` the red checks that brought the run here; each must
/// be among the diagnosed infra failures. Pure.
pub(super) fn judge_run(
    run_id: u64,
    head_sha: &str,
    doc: &Diagnosis,
    jobs: &[Job],
    approval_context: &str,
    candidates: &[&str],
) -> RunVerdict {
    let run_jobs: Vec<&Job> = jobs
        .iter()
        .filter(|job| job.run_id == Some(run_id))
        .collect();
    if run_jobs.is_empty() {
        return RunVerdict::Decline("run has no readable jobs".to_owned());
    }
    if run_jobs.iter().any(|job| {
        job.head_sha
            .as_deref()
            .is_some_and(|sha| !sha.eq_ignore_ascii_case(head_sha))
            || job
                .head_branch
                .as_deref()
                .is_some_and(|branch| branch.starts_with("gh-readonly-queue/"))
    }) {
        return RunVerdict::Decline("run is not of the current pull-request head".to_owned());
    }
    if let Some(job) = run_jobs
        .iter()
        .find(|job| is_approval_context(&job.name, approval_context))
    {
        return RunVerdict::Decline(format!(
            "the run contains {}; a re-run could re-run it",
            job.name
        ));
    }
    if run_jobs
        .iter()
        .any(|job| job.status.as_deref() != Some("completed"))
    {
        return RunVerdict::InFlight;
    }
    if !doc.omitted_checks.is_empty() || !doc.unreadable_checks.is_empty() {
        return RunVerdict::Decline("some failing required checks were not diagnosed".to_owned());
    }
    let infra = |job: &Job| -> Result<(), String> {
        let Some(check) = doc.checks.iter().find(|check| check.job_id == job.id) else {
            return Err(format!("{} failed and is not a required check", job.name));
        };
        if check.conclusion.as_deref() != Some("failure") {
            return Err(format!(
                "{} is {}",
                check.context,
                check.conclusion.as_deref().unwrap_or("unknown")
            ));
        }
        if check.classification.class != classify::INFRA {
            return Err(format!(
                "{} is {}/{}",
                check.context, check.classification.class, check.classification.rule
            ));
        }
        Ok(())
    };
    // Cancelled jobs (fail-fast siblings) never produced a result; every
    // other failure must be a diagnosed infra failure of a required job.
    for job in &run_jobs {
        if matches!(
            job.conclusion.as_deref(),
            Some("failure" | "timed_out" | "startup_failure" | "action_required")
        ) && let Err(why) = infra(job)
        {
            return RunVerdict::Decline(why);
        }
    }
    for name in candidates {
        let diagnosed = doc.checks.iter().any(|check| {
            check.run_id == Some(run_id)
                && check.context == *name
                && check.classification.class == classify::INFRA
        });
        if !diagnosed {
            return RunVerdict::Decline(format!("{name} was not diagnosed as infra in this run"));
        }
    }
    let attempt = run_jobs
        .iter()
        .filter_map(|job| job.run_attempt)
        .max()
        .unwrap_or(1);
    if attempt.saturating_sub(1) >= u64::from(MAX_RERUNS) {
        return RunVerdict::Capped;
    }
    RunVerdict::Rerun
}

/// One candidate's outcome this pass.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct Outcome {
    pub(super) pr: u64,
    pub(super) head_sha: String,
    pub(super) check: String,
    pub(super) run_id: u64,
    /// `rerun`, `would_rerun`, `declined`, `capped`, `refused`, `pending`,
    /// `waiting`, `in_flight`, `deferred` or `error`.
    pub(super) action: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub(super) detail: String,
}

/// One outage finding.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(super) struct Finding {
    pub(super) message: String,
    pub(super) check: String,
    pub(super) step: String,
    pub(super) prs: Vec<u64>,
    pub(super) capped: bool,
    /// When this finding was first reported (RFC 3339).
    pub(super) first_reported_at: String,
    /// First reported by this pass.
    pub(super) new: bool,
}

/// The repository report section.
#[derive(Clone, Debug, Default, Serialize, PartialEq, Eq)]
pub(crate) struct RepoStatus {
    pub(super) mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) outcomes: Vec<Outcome>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) findings: Vec<Finding>,
    /// Reads that failed (the sweep acted on what it could read).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) errors: Vec<String>,
}

/// Effects the pass needs from GitHub, so the policy is testable offline.
pub(super) trait Acts {
    /// Current-head check runs of `heads` (full SHAs), keyed by head.
    fn rollups(&self, heads: &[String]) -> Result<BTreeMap<String, Vec<StewardCheck>>, String>;
    /// Diagnose the latest attempt of `run_id`.
    fn diagnose(&self, run_id: u64) -> Result<(Diagnosis, Vec<Job>), String>;
    /// `POST /actions/runs/{run_id}/rerun-failed-jobs`.
    fn rerun_failed_jobs(&self, run_id: u64) -> Result<(), String>;
}

/// [`Acts`] over the steward's scoped GitHub client.
pub(super) struct GitHubActs<'a> {
    pub(super) actions: &'a GitHubActions,
    pub(super) repo: &'a str,
    pub(super) required: Vec<String>,
    pub(super) deadline: Option<Instant>,
}

impl Acts for GitHubActs<'_> {
    fn rollups(&self, heads: &[String]) -> Result<BTreeMap<String, Vec<StewardCheck>>, String> {
        let mut out = BTreeMap::new();
        for batch in heads.chunks(super::head_rollup::HEADS_PER_QUERY) {
            let read =
                super::head_rollup::read_rollups(self.actions, self.repo, batch, self.deadline)?;
            out.extend(
                read.into_iter()
                    .map(|(head, rollup)| (head, rollup.check_runs)),
            );
        }
        Ok(out)
    }

    fn diagnose(&self, run_id: u64) -> Result<(Diagnosis, Vec<Job>), String> {
        let read = |args: &[String]| self.actions.run_gh(args).map_err(|error| error.to_string());
        crate::app::diagnose_cmd::diagnose_runs(&read, self.repo, &[run_id], &self.required)
    }

    fn rerun_failed_jobs(&self, run_id: u64) -> Result<(), String> {
        self.actions
            .rerun_failed_jobs(self.repo, run_id)
            .map_err(|error| error.to_string())
    }
}

/// Inputs of one repository's sweep.
pub(super) struct Sweep<'a> {
    pub(super) observation: &'a RepoObservation,
    pub(super) mode: Mode,
    pub(super) approval_context: &'a str,
    pub(super) excluded_labels: &'a [String],
    pub(super) now_epoch: i64,
    /// Past this the sweep reads nothing new.
    pub(super) deadline: Option<Instant>,
    /// Where a re-run's intent is saved before the POST (apply passes only).
    pub(super) ledger_path: Option<&'a Path>,
}

fn diagnosis_of<'a>(doc: &'a Diagnosis, run_id: u64, check: &str) -> Option<&'a CheckDiagnosis> {
    doc.checks
        .iter()
        .find(|item| item.run_id == Some(run_id) && item.context == check)
}

fn step_of(doc: &Diagnosis, run_id: u64, check: &str) -> String {
    match diagnosis_of(doc, run_id, check).and_then(|item| item.failing_step.as_ref()) {
        Some(step) => step.name.clone(),
        None => NO_STEP.to_owned(),
    }
}

fn tests_of(doc: &Diagnosis, run_id: u64, check: &str) -> Vec<String> {
    let mut names = diagnosis_of(doc, run_id, check)
        .map(|item| item.failing_tests.names.clone())
        .unwrap_or_default();
    names.sort();
    names.dedup();
    names
}

fn rule_of(doc: &Diagnosis, run_id: u64, check: &str) -> String {
    diagnosis_of(doc, run_id, check)
        .map(|item| format!("{}/{}", item.classification.class, item.classification.rule))
        .unwrap_or_default()
}

fn record_for<'a>(
    ledger: &'a mut StewardLedger,
    repo: &str,
    candidate: &Candidate,
) -> &'a mut Record {
    let record = ledger
        .infra_reruns
        .entry(record_key(
            repo,
            candidate.pr,
            &candidate.head_sha,
            &candidate.check,
        ))
        .or_default();
    record.pr = candidate.pr;
    record.check.clone_from(&candidate.check);
    if record.judged_check_id != Some(candidate.check_id) {
        record.errors = 0;
        // A newer red: the older red's test names no longer describe it. They
        // come back only from this red's own diagnosis, so a failed diagnosis
        // never leaves stale names under a matching id (red_everywhere).
        record.tests.clear();
    }
    record.judged_check_id = Some(candidate.check_id);
    record
}

fn deadline_passed(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|deadline| Instant::now() >= deadline)
}

/// Run one repository's sweep: judge each red candidate, re-run within the
/// caps (enforce) or report what would be re-run (shadow), prune records of
/// heads no longer open, and report outage findings. Returns the report
/// section and whether a GitHub write failed.
#[allow(clippy::too_many_lines)]
pub(super) fn sweep(
    input: &Sweep<'_>,
    ledger: &mut StewardLedger,
    acts: &dyn Acts,
) -> (RepoStatus, bool) {
    let mut status = RepoStatus {
        mode: input.mode.label().to_owned(),
        ..RepoStatus::default()
    };
    let repo = &input.observation.repo;
    prune(ledger, input.observation, input.now_epoch);
    if input.mode == Mode::Off {
        return (status, false);
    }
    // Every head's checks: the census's where it read them, else one batched
    // rollup read for the heads it skipped.
    let mut checks: BTreeMap<u64, Vec<StewardCheck>> = BTreeMap::new();
    let unhydrated = unhydrated_heads(input.observation, input.excluded_labels);
    let mut hydrated = BTreeMap::new();
    if !unhydrated.is_empty() && !deadline_passed(input.deadline) {
        match acts.rollups(&unhydrated) {
            Ok(read) => hydrated = read,
            Err(error) => status.errors.push(format!("check rollups: {error}")),
        }
    }
    for pr in &input.observation.prs {
        if !pr.fact.checks.is_empty() {
            checks.insert(pr.fact.number, pr.fact.checks.clone());
        } else if let Some(read) = hydrated.get(&pr.fact.head_sha) {
            checks.insert(pr.fact.number, read.clone());
        }
    }
    let found = candidates(
        input.observation,
        &checks,
        input.approval_context,
        input.excluded_labels,
    );
    // Group by run: one diagnosis and one re-run cover every check of a run.
    let mut runs: BTreeMap<u64, Vec<&Candidate>> = BTreeMap::new();
    for candidate in &found {
        runs.entry(candidate.run_id).or_default().push(candidate);
    }
    let mut failed = false;
    let mut diagnosed = 0usize;
    let mut stop_reading: Option<String> = None;
    // Newest runs first, so old reds never starve a fresh outage.
    for (run_id, members) in runs.into_iter().rev() {
        let push =
            |status: &mut RepoStatus, candidate: &Candidate, action: &str, detail: String| {
                status.outcomes.push(Outcome {
                    pr: candidate.pr,
                    head_sha: candidate.head_sha.clone(),
                    check: candidate.check.clone(),
                    run_id,
                    action: action.to_owned(),
                    detail,
                });
            };
        let nexts: Vec<Next> = members
            .iter()
            .map(|candidate| {
                let key = record_key(repo, candidate.pr, &candidate.head_sha, &candidate.check);
                next(
                    ledger.infra_reruns.get(&key),
                    candidate.check_id,
                    input.now_epoch,
                    input.mode,
                )
            })
            .collect();
        // One member's backoff holds the whole run: a re-run re-runs them all.
        if let Some(Next::Wait { remaining_secs }) =
            nexts.iter().find(|next| matches!(next, Next::Wait { .. }))
        {
            for candidate in &members {
                push(
                    &mut status,
                    candidate,
                    "waiting",
                    format!("backoff: {remaining_secs}s left"),
                );
            }
            continue;
        }
        if !nexts.contains(&Next::Diagnose) {
            for (candidate, next) in members.iter().zip(&nexts) {
                let (action, detail) = match next {
                    Next::Settled(verdict) => {
                        let why = ledger
                            .infra_reruns
                            .get(&record_key(
                                repo,
                                candidate.pr,
                                &candidate.head_sha,
                                &candidate.check,
                            ))
                            .map(|record| record.why.clone())
                            .unwrap_or_default();
                        (verdict.as_str(), why)
                    }
                    _ => ("pending", String::new()),
                };
                push(&mut status, candidate, action, detail);
            }
            continue;
        }
        // Free: the census already shows the run still going.
        if members.iter().any(|candidate| candidate.run_in_flight) {
            for candidate in &members {
                push(&mut status, candidate, "in_flight", String::new());
            }
            continue;
        }
        let spent = if let Some(reason) = &stop_reading {
            Some(reason.clone())
        } else if deadline_passed(input.deadline) {
            Some("the pass time budget is spent".to_owned())
        } else if diagnosed >= MAX_RUN_DIAGNOSES {
            Some("the diagnosis budget for this pass is spent".to_owned())
        } else {
            None
        };
        if let Some(reason) = spent {
            for candidate in &members {
                push(&mut status, candidate, "deferred", reason.clone());
            }
            continue;
        }
        diagnosed += 1;
        let head_sha = &members[0].head_sha;
        let (doc, jobs) = match acts.diagnose(run_id) {
            Ok(read) => read,
            Err(error) => {
                if super::pass_budget::is_rate_limited(&error) {
                    stop_reading = Some("GitHub rate-limited the sweep".to_owned());
                }
                for candidate in &members {
                    let record = record_for(ledger, repo, candidate);
                    record.errors += 1;
                    record.last_error_epoch = Some(input.now_epoch);
                    "error".clone_into(&mut record.verdict);
                    record.why = format!("diagnosis: {error}");
                    push(&mut status, candidate, "error", record.why.clone());
                }
                continue;
            }
        };
        let names: Vec<&str> = members
            .iter()
            .map(|candidate| candidate.check.as_str())
            .collect();
        match judge_run(
            run_id,
            head_sha,
            &doc,
            &jobs,
            input.approval_context,
            &names,
        ) {
            RunVerdict::InFlight => {
                for candidate in &members {
                    push(&mut status, candidate, "in_flight", String::new());
                }
            }
            RunVerdict::Capped => {
                for candidate in &members {
                    let record = record_for(ledger, repo, candidate);
                    record.reruns = record.reruns.max(MAX_RERUNS);
                    "capped".clone_into(&mut record.verdict);
                    record.step = step_of(&doc, run_id, &candidate.check);
                    record.tests = tests_of(&doc, run_id, &candidate.check);
                    record.why = format!(
                        "re-run {MAX_RERUNS} times and still {}",
                        rule_of(&doc, run_id, &candidate.check)
                    );
                    push(&mut status, candidate, "capped", record.why.clone());
                }
            }
            RunVerdict::Decline(reason) => {
                for candidate in &members {
                    let record = record_for(ledger, repo, candidate);
                    "declined".clone_into(&mut record.verdict);
                    record.step = step_of(&doc, run_id, &candidate.check);
                    record.tests = tests_of(&doc, run_id, &candidate.check);
                    record.why.clone_from(&reason);
                    push(&mut status, candidate, "declined", reason.clone());
                }
            }
            RunVerdict::Rerun => {
                // A record whose budget is spent caps the run, whatever
                // GitHub's attempt counter says (a fresh run of the check on
                // the same head starts again at attempt 1).
                let spent = members.iter().any(|candidate| {
                    ledger
                        .infra_reruns
                        .get(&record_key(
                            repo,
                            candidate.pr,
                            &candidate.head_sha,
                            &candidate.check,
                        ))
                        .is_some_and(|record| record.reruns >= MAX_RERUNS)
                });
                for candidate in &members {
                    let rule = rule_of(&doc, run_id, &candidate.check);
                    let step = step_of(&doc, run_id, &candidate.check);
                    let tests = tests_of(&doc, run_id, &candidate.check);
                    let record = record_for(ledger, repo, candidate);
                    record.step.clone_from(&step);
                    record.tests = tests;
                    record.why.clone_from(&rule);
                    if spent {
                        "capped".clone_into(&mut record.verdict);
                    } else if input.mode == Mode::Enforce {
                        // Intent first: a crash after the POST must not
                        // forget the attempt.
                        record.reruns += 1;
                        record.last_rerun_epoch = Some(input.now_epoch);
                        "rerun".clone_into(&mut record.verdict);
                    } else {
                        "would_rerun".clone_into(&mut record.verdict);
                    }
                }
                if spent {
                    for candidate in &members {
                        push(&mut status, candidate, "capped", String::new());
                    }
                    continue;
                }
                let outcome = if input.mode == Mode::Enforce {
                    let saved = input.ledger_path.map_or(Ok(()), |path| {
                        super::save_ledger(path, ledger).map_err(|error| error.message().to_owned())
                    });
                    match saved.and_then(|()| acts.rerun_failed_jobs(run_id)) {
                        Ok(()) => Ok("rerun"),
                        Err(error) => Err(error),
                    }
                } else {
                    Ok("would_rerun")
                };
                for candidate in &members {
                    let record = record_for(ledger, repo, candidate);
                    let detail = format!("{} at step '{}'", record.why, record.step);
                    match &outcome {
                        Ok(action) => push(&mut status, candidate, action, detail),
                        Err(error) => {
                            failed = true;
                            record.reruns = record.reruns.saturating_sub(1);
                            record.last_rerun_epoch = None;
                            record.errors += 1;
                            record.last_error_epoch = Some(input.now_epoch);
                            "error".clone_into(&mut record.verdict);
                            push(
                                &mut status,
                                candidate,
                                "error",
                                format!("rerun-failed-jobs: {error}"),
                            );
                        }
                    }
                }
            }
        }
    }
    status.findings = findings(ledger, input.observation, &checks, input.now_epoch);
    (status, failed)
}

/// Drop this repository's records for heads no longer open, and outage
/// findings unseen for [`OUTAGE_GRACE_SECS`].
fn prune(ledger: &mut StewardLedger, observation: &RepoObservation, now_epoch: i64) {
    let prefix = repo_prefix(&observation.repo);
    let live: BTreeSet<String> = observation
        .prs
        .iter()
        .map(|pr| {
            format!(
                "{prefix}{}@{}:",
                pr.fact.number,
                pr.fact.head_sha.to_ascii_lowercase()
            )
        })
        .collect();
    ledger.infra_reruns.retain(|key, _| {
        !key.starts_with(&prefix) || live.iter().any(|head| key.starts_with(head.as_str()))
    });
    let outage_prefix = format!("{}|", observation.repo.to_ascii_lowercase());
    ledger.infra_outages.retain(|key, seen| {
        !key.starts_with(&outage_prefix) || now_epoch - seen.last_seen_epoch < OUTAGE_GRACE_SECS
    });
}

/// Whether the newest observation of `check` on a head has not passed: red,
/// or re-running.
fn still_failing(checks: &[StewardCheck], check: &str) -> bool {
    latest_checks(checks)
        .into_iter()
        .find(|item| item.name == check)
        .is_some_and(|item| {
            !is(Some(&item.status), "completed")
                || !matches!(
                    item.conclusion
                        .as_deref()
                        .map(str::to_ascii_lowercase)
                        .as_deref(),
                    Some("success" | "neutral" | "skipped")
                )
        })
}

/// Outage findings: open pull requests whose check the ledger judged infra
/// (re-run, would re-run, or capped) and which has not passed since, grouped
/// by check and step. A group is a finding when any member is capped or it
/// has [`OUTAGE_MIN_PRS`] members. Each finding's first report is remembered
/// while it lasts (plus a grace period), so it is reported as one finding.
fn findings(
    ledger: &mut StewardLedger,
    observation: &RepoObservation,
    checks: &BTreeMap<u64, Vec<StewardCheck>>,
    now_epoch: i64,
) -> Vec<Finding> {
    let prefix = repo_prefix(&observation.repo);
    let mut groups: BTreeMap<(String, String), (BTreeSet<u64>, bool)> = BTreeMap::new();
    for (key, record) in &ledger.infra_reruns {
        if !key.starts_with(&prefix)
            || !matches!(record.verdict.as_str(), "rerun" | "would_rerun" | "capped")
            || record.step.is_empty()
        {
            continue;
        }
        let Some(head_checks) = checks.get(&record.pr) else {
            continue;
        };
        if !still_failing(head_checks, &record.check) {
            continue;
        }
        let entry = groups
            .entry((record.check.clone(), record.step.clone()))
            .or_default();
        entry.0.insert(record.pr);
        entry.1 |= record.verdict == "capped";
    }
    let outage_prefix = format!("{}|", observation.repo.to_ascii_lowercase());
    let mut out = Vec::new();
    for ((check, step), (prs, capped)) in groups {
        if !capped && prs.len() < OUTAGE_MIN_PRS {
            continue;
        }
        let key = format!("{outage_prefix}{check}|{step}");
        let new = !ledger.infra_outages.contains_key(&key);
        let seen = ledger.infra_outages.entry(key).or_insert(OutageSeen {
            first_epoch: now_epoch,
            last_seen_epoch: now_epoch,
        });
        seen.last_seen_epoch = now_epoch;
        let first = seen.first_epoch;
        out.push(Finding {
            message: format!(
                "infra outage suspected: {check} failing on {} PRs at step '{step}'",
                prs.len()
            ),
            check,
            step,
            prs: prs.into_iter().collect(),
            capped,
            first_reported_at: chrono::DateTime::from_timestamp(first, 0)
                .map(|at| at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                .unwrap_or_default(),
            new,
        });
    }
    out
}

#[cfg(test)]
mod tests;
