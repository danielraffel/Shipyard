//! Jobs GitHub never gives a runner: the queue head's merge-group jobs and,
//! report only, the required-check jobs of approved pull request heads.
//!
//! Under an `ALLGREEN` merge queue every entry behind the head is built on
//! the head's group, so a head job that stays `queued` with no runner holds
//! the whole queue until the ruleset's check timeout. On 2026-11-28 Vellum
//! run 46104476461 (#825) had job `gpu-macos-arm64` 140854026289 queued with
//! runner id 0 for 69 minutes while job 140859201898 (run 46106139411, #791,
//! same labels, created 04:08:34) started on a runner at 04:11:24. It was
//! cancelled by hand. A head cancel is not free: #825 was ejected, the groups
//! behind it were rebuilt (discarding #769's finished 58-minute GPU job), and
//! #825 was re-enqueued 37 minutes later.
//!
//! A runner going to a job created in the same batch proves only that there
//! were fewer runners than jobs. At 07:38Z the same day the head job
//! 140908713155 (created 07:38:19) waited while its batch sibling 140908717862
//! (#816, created 07:38:21) started at 07:44:01; the head got a runner at
//! 08:10:19 and #825 merged at 09:03:46 unaided. A witness must therefore be
//! created at least [`WITNESS_MARGIN_SECS`] after the stalled job.
//!
//! **Detect** (report only; [`detect`]). Targets:
//!
//! * every merge-queue base: the queue head entry only, each unfinished
//!   `merge_group` run on its current group head;
//! * only when `pr_stall_recover` is not `off`: each open, non-draft pull
//!   request whose head the approval context (`review-verdict`) reports
//!   green, each `pull_request` run on its exact head still `queued` as a
//!   whole. Pull request stalls are never acted on: the seven Vellum PR jobs
//!   queued from 05:12Z on 2026-11-28 were a backlog on their label served
//!   out of order, and a cancel and re-run only resets a job's place.
//!
//! A target run is read only once it is older than the threshold, at most
//! [`MAX_TARGET_READS`] per pass. A job is stalled when it is `queued`, has
//! no runner (runner id absent or 0 and no runner name), names labels, has
//! waited at least `[auto_merge] mq_stall_minutes` (default 20) since it was
//! created and no other job of its run is `in_progress`. A pull request job
//! must also produce a required check. A stalled job counts only with a
//! witness: a job of the same repository created at least
//! [`WITNESS_MARGIN_SECS`] after it, whose labels include all of the stalled
//! job's, that started on a runner. The witness scan lists the stalled
//! workflow's runs created since the oldest stalled job (one read; completed
//! runs count) and joins the pass's run census; it reads the stalled runs'
//! own jobs first, then the same workflow, then other merge groups, then the
//! rest, skipping runs still `queued`, at most [`MAX_WITNESS_READS`] job
//! reads, and stops once every stall has a witness. Jobs are read once per
//! pass. Any failed or partial read decides nothing and is reported as
//! unread. The summary tokens are `mq_stall=#<pr>:run:<id>:job:<id>:<age>m`
//! and `pr_stall=...`.
//!
//! **Recover** (`[auto_merge] mq_stall_recover`, `off | shadow | apply`,
//! default off; queue head only). `shadow`, or `apply` on a pass that does
//! not apply, reports `would_cancel` and writes nothing; a pull request stall
//! only ever reports `would_cancel`. Under `apply` the steward cancels the
//! stalled run once per run id, under the run mutation guard, after a fresh
//! read shows the entry is still the head on the same group and the job is
//! still queued with no runner. At most one cancel per repository per pass,
//! at most one per pull request in [`PER_PR_WINDOW_SECS`] (a re-enqueued head
//! gets a new run id), and at most `mq_stall_max_cancels_per_hour` (default
//! 3) per repository per hour. Every attempt is a receipt in the steward
//! ledger.
//!
//! **Prove** ([`progress`]), on a later pass, from a fresh run read:
//! recovered only when the run concluded `cancelled` and the pull request
//! moved: it left the open set, sits in the queue on a different group, has
//! a newer merge-group run, or this pass enqueued it again. Until
//! [`RECOVERY_WINDOW_SECS`] after the act a receipt that has not moved stays
//! pending. After it the receipt becomes `unrecovered`
//! (`mq_stall_unrecovered`) and that run is never cancelled again. A failed
//! run read keeps the receipt pending for at most [`PROGRESS_UNREAD_SECS`].

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{MutationControl, PrReport, RepoObservation, StewardLedger};
use crate::merge_steward::StewardRun;
use crate::merge_steward::mq_stall::{MqStallMode, MqStallPolicy};

/// The most target-run job reads one pass spends.
pub(super) const MAX_TARGET_READS: usize = 15;
/// The most job reads one pass spends looking for witnesses.
pub(super) const MAX_WITNESS_READS: usize = 10;
/// A receipt's outcome while its cancel is in flight; saved before the cancel.
pub(super) const INTENT: &str = "pending";
/// What a later pass reads an intent with no outcome as: the cancel may or
/// may not have landed. It counts against every limit like any attempt.
pub(super) const CANCEL_UNKNOWN: &str = "cancel_unknown";
/// A witness must be created at least this long after the stalled job, so a
/// batch sibling taking the one free runner never counts.
pub(super) const WITNESS_MARGIN_SECS: i64 = 300;
/// At most one stall cancel per pull request in this window.
pub(super) const PER_PR_WINDOW_SECS: i64 = 6 * 3600;
/// A receipt is proved no sooner than this after its last act.
pub(super) const PROGRESS_MIN_SECS: i64 = 60;
/// How long the entry may take to move after the act before the receipt is
/// unrecovered. Vellum #825, cancelled by hand at 04:56Z on 2026-11-28, was
/// ejected at 04:57 and re-enqueued by the steward at 05:33.
pub(super) const RECOVERY_WINDOW_SECS: i64 = 3600;
/// How long a receipt may wait on a run read that keeps failing.
pub(super) const PROGRESS_UNREAD_SECS: i64 = 3600;
/// Receipts older than this are dropped once proved.
const RECEIPT_RETENTION_DAYS: i64 = 7;

/// Which stall.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Kind {
    /// The queue head's merge-group run.
    #[default]
    MergeQueue,
    /// An approved pull request head's required-check run.
    PullRequest,
}

impl Kind {
    fn mode(self, policy: &MqStallPolicy) -> MqStallMode {
        match self {
            Self::MergeQueue => policy.mode,
            Self::PullRequest => policy.pr_mode,
        }
    }
}

/// One workflow job, as much as the stall needs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Job {
    pub(super) id: u64,
    pub(super) run_id: u64,
    pub(super) name: String,
    pub(super) status: String,
    pub(super) created_at: Option<DateTime<Utc>>,
    pub(super) started_at: Option<DateTime<Utc>>,
    pub(super) runner_id: Option<u64>,
    pub(super) runner_name: Option<String>,
    pub(super) labels: Vec<String>,
}

impl Job {
    /// GitHub gave the job a runner.
    pub(super) fn has_runner(&self) -> bool {
        self.runner_id.is_some_and(|id| id > 0)
            || self
                .runner_name
                .as_deref()
                .is_some_and(|name| !name.trim().is_empty())
    }

    /// Queued and waiting for a runner.
    pub(super) fn waiting(&self) -> bool {
        self.status.eq_ignore_ascii_case("queued") && !self.has_runner()
    }

    /// Whether this job's labels include every one of `wanted`'s.
    fn covers(&self, wanted: &[String]) -> bool {
        wanted.iter().all(|label| {
            self.labels
                .iter()
                .any(|have| have.eq_ignore_ascii_case(label))
        })
    }

    /// Whether this job proves runners for `stalled`'s labels were handed
    /// out after `stalled` was queued: created at least
    /// [`WITNESS_MARGIN_SECS`] later (not a batch sibling), with every one of
    /// its labels, and started on a runner.
    fn witnesses(&self, stalled: &Job) -> bool {
        let Some(created) = stalled.created_at else {
            return false;
        };
        let margin = created + chrono::Duration::seconds(WITNESS_MARGIN_SECS);
        self.id != stalled.id
            && !stalled.labels.is_empty()
            && self.has_runner()
            && self.created_at.is_some_and(|at| at >= margin)
            && self.started_at.is_some_and(|at| at > created)
            && self.covers(&stalled.labels)
    }
}

fn time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(Value::as_str)
        .and_then(|text| DateTime::parse_from_rfc3339(text).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// One job row.
/// A field that is absent or `null`, or of its type; any other type makes the
/// whole row unreadable, never a default.
fn optional<'a, T>(
    row: &'a Value,
    key: &str,
    read: impl Fn(&'a Value) -> Option<T>,
) -> Result<Option<T>, String> {
    match row.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => read(value)
            .map(Some)
            .ok_or_else(|| format!("a workflow job's `{key}` has the wrong type")),
    }
}

/// One job row. A malformed runner field or label is unreadable: it must
/// never read as "no runner" or narrow the labels.
pub(super) fn parse_job(row: &Value) -> Result<Job, String> {
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let labels = match row.get("labels") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(labels)) => labels
            .iter()
            .map(|label| {
                label
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "a workflow job has a label that is not a string".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err("a workflow job's `labels` is not a list".to_owned()),
    };
    Ok(Job {
        id: row
            .get("id")
            .and_then(Value::as_u64)
            .filter(|id| *id > 0)
            .ok_or_else(|| "a workflow job had no id".to_owned())?,
        run_id: row.get("run_id").and_then(Value::as_u64).unwrap_or(0),
        name: text("name").unwrap_or_default(),
        status: text("status").ok_or_else(|| "a workflow job had no status".to_owned())?,
        created_at: time(row.get("created_at")),
        started_at: time(row.get("started_at")),
        runner_id: optional(row, "runner_id", Value::as_u64)?,
        runner_name: optional(row, "runner_name", |value| {
            value.as_str().map(str::to_owned)
        })?,
        labels,
    })
}

fn read_value(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    path: &str,
) -> Result<Value, String> {
    let raw = run_gh(&["api".to_owned(), path.to_owned()])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from `{path}`: {error}"))
}

/// The jobs of one run's latest attempt, one page, proved complete.
pub(super) fn read_jobs(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    run_id: u64,
) -> Result<Vec<Job>, String> {
    let value = read_value(
        run_gh,
        &format!("repos/{repo}/actions/runs/{run_id}/jobs?filter=latest&per_page=100"),
    )?;
    let rows = value
        .get("jobs")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("run {run_id} jobs answer had no jobs"))?;
    super::merge_group_stall::complete_page(&value, rows.len(), "workflow jobs")?;
    rows.iter().map(parse_job).collect()
}

/// A workflow's runs created at or after `since`, newest page only: a
/// witness source, so a partial page can only miss evidence, never invent it.
pub(super) fn read_workflow_runs(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    repo: &str,
    workflow_id: u64,
    since: DateTime<Utc>,
) -> Result<Vec<StewardRun>, String> {
    let value = read_value(
        run_gh,
        &format!(
            "repos/{repo}/actions/workflows/{workflow_id}/runs?created=%3E%3D{}&per_page=100",
            since.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        ),
    )?;
    let rows = value
        .get("workflow_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| "workflow runs answer had no workflow_runs".to_owned())?;
    rows.iter()
        .map(|row| {
            super::observation::parse_run(row)
                .ok_or_else(|| "workflow runs answer held a malformed run".to_owned())
        })
        .collect()
}

/// The job that waits, and the job that proves runners were being handed out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Stall {
    pub(super) kind: Kind,
    pub(super) pr: u64,
    /// The queue position read this pass (merge queue only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) position: Option<u64>,
    /// The group head (merge queue) or the pull request head.
    pub(super) head_sha: String,
    pub(super) run_id: u64,
    pub(super) job_id: u64,
    pub(super) job_name: String,
    pub(super) labels: Vec<String>,
    pub(super) created_at: DateTime<Utc>,
    pub(super) age_minutes: i64,
    pub(super) witness_run_id: u64,
    pub(super) witness_job_id: u64,
    pub(super) witness_runner: Option<String>,
    pub(super) witness_started_at: DateTime<Utc>,
}

impl Stall {
    /// `#<pr>:run:<id>:job:<id>:<age>m`.
    pub(super) fn token(&self) -> String {
        format!(
            "#{}:run:{}:job:{}:{}m",
            self.pr, self.run_id, self.job_id, self.age_minutes
        )
    }
}

/// This pass's reading.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Detection {
    /// The queue head's stall first, then pull requests, oldest job first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) stalls: Vec<Stall>,
    /// A read failed or was partial: nothing was decided.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) unread: Option<String>,
    /// Why a waiting job did not count, when one did not.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) notes: Vec<String>,
    /// Job and run-list reads spent.
    pub(super) reads: usize,
}

fn run_created(run: &StewardRun) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&run.created_at)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// The queue head: the entry with the lowest position.
fn queue_head(observation: &RepoObservation) -> Option<(u64, u64)> {
    observation
        .prs
        .iter()
        .filter_map(|pr| pr.fact.queue_position.map(|at| (at, pr.fact.number)))
        .min()
        .map(|(position, pr)| (pr, position))
}

/// One run to read: whose it is, and what a stall in it would be.
struct Target<'a> {
    kind: Kind,
    pr: u64,
    position: Option<u64>,
    head_sha: String,
    run: &'a StewardRun,
    /// Pull request only: the run ids of the head's required checks.
    required_runs: Vec<u64>,
}

fn targets<'a>(
    observation: &'a RepoObservation,
    approval_context: Option<&str>,
    threshold: chrono::Duration,
    now: DateTime<Utc>,
) -> Vec<Target<'a>> {
    let old = |run: &StewardRun| run_created(run).is_some_and(|at| now - at >= threshold);
    let mut out = Vec::new();
    if let Some((pr, position)) = queue_head(observation)
        && let Some(group) = observation.merge_group_heads.get(&pr)
    {
        let mut runs = observation
            .runs
            .iter()
            .filter(|run| {
                run.event == "merge_group"
                    && !run.status.eq_ignore_ascii_case("completed")
                    && run.head_sha.eq_ignore_ascii_case(group)
                    && super::merge_group_pr_number(run) == Some(pr)
                    && old(run)
            })
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| run.id);
        out.extend(runs.into_iter().map(|run| Target {
            kind: Kind::MergeQueue,
            pr,
            position: Some(position),
            head_sha: group.clone(),
            run,
            required_runs: Vec::new(),
        }));
    }
    // Pull request targets cost reads; only a base that asks for them pays.
    let Some(approval_context) = approval_context else {
        return out;
    };
    let required = observation
        .required_checks
        .iter()
        .map(|rule| rule.context.as_str())
        .collect::<Vec<_>>();
    let mut prs = observation
        .prs
        .iter()
        .filter(|pr| pr.fact.queue_position.is_none() && !pr.fact.draft)
        .filter(|pr| {
            pr.fact.checks.iter().any(|check| {
                check.name.eq_ignore_ascii_case(approval_context.trim())
                    && check
                        .conclusion
                        .as_deref()
                        .is_some_and(|conclusion| conclusion.eq_ignore_ascii_case("success"))
            })
        })
        .collect::<Vec<_>>();
    prs.sort_by_key(|pr| pr.fact.number);
    for pr in prs {
        let required_runs = pr
            .fact
            .checks
            .iter()
            .filter(|check| required.contains(&check.name.as_str()))
            .filter_map(|check| check.run_id)
            .collect::<Vec<_>>();
        let mut runs = observation
            .runs
            .iter()
            .filter(|run| {
                run.event == "pull_request"
                    && run.status.eq_ignore_ascii_case("queued")
                    && run.head_sha.eq_ignore_ascii_case(&pr.fact.head_sha)
                    && run
                        .pull_request_number
                        .is_none_or(|number| number == pr.fact.number)
                    && old(run)
            })
            .collect::<Vec<_>>();
        runs.sort_by_key(|run| run.id);
        out.extend(runs.into_iter().map(|run| Target {
            kind: Kind::PullRequest,
            pr: pr.fact.number,
            position: None,
            head_sha: pr.fact.head_sha.clone(),
            run,
            required_runs: required_runs.clone(),
        }));
    }
    out
}

struct Reader<'a> {
    run_gh: &'a dyn Fn(&[String]) -> Result<String, String>,
    repo: &'a str,
    cache: BTreeMap<u64, Vec<Job>>,
    reads: usize,
}

impl Reader<'_> {
    fn jobs(&mut self, run_id: u64) -> Result<Vec<Job>, String> {
        if let Some(jobs) = self.cache.get(&run_id) {
            return Ok(jobs.clone());
        }
        self.reads += 1;
        let jobs = read_jobs(self.run_gh, self.repo, run_id)?;
        self.cache.insert(run_id, jobs.clone());
        Ok(jobs)
    }
}

/// Read the targets' jobs and, when one waits, look for witnesses. Reads
/// nothing unless a target run is older than the threshold. `approval_context`
/// is `None` when pull request targets are off.
#[allow(clippy::too_many_lines)] // Targets, candidates and the witness scan read in one order.
pub(super) fn detect(
    run_gh: &dyn Fn(&[String]) -> Result<String, String>,
    observation: &RepoObservation,
    approval_context: Option<&str>,
    minutes: u32,
    now: DateTime<Utc>,
) -> Detection {
    let mut detection = Detection::default();
    if !observation.merge_queue {
        return detection;
    }
    let threshold = chrono::Duration::minutes(i64::from(minutes));
    let targets = targets(observation, approval_context, threshold, now);
    if targets.is_empty() {
        return detection;
    }
    let mut reader = Reader {
        run_gh,
        repo: &observation.repo,
        cache: BTreeMap::new(),
        reads: 0,
    };
    let mut waiting: Vec<(&Target<'_>, Job)> = Vec::new();
    for (index, target) in targets.iter().enumerate() {
        if index >= MAX_TARGET_READS {
            detection.notes.push(format!(
                "{} target runs; read the first {MAX_TARGET_READS}",
                targets.len()
            ));
            break;
        }
        let jobs = match reader.jobs(target.run.id) {
            Ok(jobs) => jobs,
            Err(error) => {
                detection.unread = Some(format!("run {} jobs: {error}", target.run.id));
                detection.reads = reader.reads;
                return detection;
            }
        };
        let sibling_running = jobs
            .iter()
            .any(|job| job.status.eq_ignore_ascii_case("in_progress"));
        for job in jobs.iter().filter(|job| job.waiting()) {
            let Some(created) = job.created_at else {
                continue;
            };
            if now - created < threshold || job.labels.is_empty() {
                continue;
            }
            if sibling_running {
                detection.notes.push(format!(
                    "job {} waits, but another job of run {} is in progress",
                    job.id, target.run.id
                ));
                continue;
            }
            if target.kind == Kind::PullRequest
                && !target.required_runs.contains(&target.run.id)
                && !observation
                    .required_checks
                    .iter()
                    .any(|rule| rule.context.eq_ignore_ascii_case(&job.name))
            {
                detection.notes.push(format!(
                    "job {} waits, but produces no required check of #{}",
                    job.id, target.pr
                ));
                continue;
            }
            waiting.push((target, job.clone()));
        }
    }
    if waiting.is_empty() {
        detection.reads = reader.reads;
        return detection;
    }
    // Oldest stalled job decides how far back witnesses can come from.
    let Some(oldest) = waiting.iter().filter_map(|(_, job)| job.created_at).min() else {
        detection.reads = reader.reads;
        return detection;
    };
    let mut runs: Vec<StewardRun> = observation.runs.clone();
    let mut workflows = waiting
        .iter()
        .map(|(target, _)| target.run.workflow_id)
        .collect::<Vec<_>>();
    workflows.sort_unstable();
    workflows.dedup();
    for workflow in &workflows {
        reader.reads += 1;
        match read_workflow_runs(run_gh, &observation.repo, *workflow, oldest) {
            Ok(listed) => {
                for run in listed {
                    if !runs.iter().any(|known| known.id == run.id) {
                        runs.push(run);
                    }
                }
            }
            Err(error) => {
                detection.unread = Some(format!("workflow {workflow} runs: {error}"));
                detection.reads = reader.reads;
                return detection;
            }
        }
    }
    let stalled_runs = waiting
        .iter()
        .map(|(target, _)| target.run.id)
        .collect::<Vec<_>>();
    let oldest_run = waiting
        .iter()
        .filter_map(|(target, _)| run_created(target.run))
        .min();
    let mut candidates = runs
        .iter()
        .filter(|run| {
            stalled_runs.contains(&run.id)
                || (!run.status.eq_ignore_ascii_case("queued") && run_created(run) >= oldest_run)
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|run| {
        let rank = if stalled_runs.contains(&run.id) {
            0
        } else if workflows.contains(&run.workflow_id) {
            1
        } else if run.event == "merge_group" {
            2
        } else {
            3
        };
        (rank, run.created_at.clone(), run.id)
    });
    let mut found: BTreeMap<u64, (u64, Job)> = BTreeMap::new();
    let mut spent = 0usize;
    for run in candidates {
        if found.len() == waiting.len() {
            break;
        }
        if !reader.cache.contains_key(&run.id) {
            if spent >= MAX_WITNESS_READS {
                detection.notes.push(format!(
                    "no witness for every waiting job in the first {MAX_WITNESS_READS} runs read"
                ));
                break;
            }
            spent += 1;
        }
        let jobs = match reader.jobs(run.id) {
            Ok(jobs) => jobs,
            Err(error) => {
                detection.unread = Some(format!("witness run {} jobs: {error}", run.id));
                detection.reads = reader.reads;
                return detection;
            }
        };
        for (_, stalled) in &waiting {
            if found.contains_key(&stalled.id) {
                continue;
            }
            if let Some(witness) = jobs.iter().find(|job| job.witnesses(stalled)) {
                found.insert(stalled.id, (run.id, witness.clone()));
            }
        }
    }
    detection.reads = reader.reads;
    let mut stalls = Vec::new();
    for (target, stalled) in &waiting {
        let Some(created) = stalled.created_at else {
            continue;
        };
        let Some((witness_run, witness)) = found.get(&stalled.id) else {
            detection.notes.push(format!(
                "job {} waits, but no newer job with its labels has started on a runner",
                stalled.id
            ));
            continue;
        };
        stalls.push(Stall {
            kind: target.kind,
            pr: target.pr,
            position: target.position,
            head_sha: target.head_sha.clone(),
            run_id: target.run.id,
            job_id: stalled.id,
            job_name: stalled.name.clone(),
            labels: stalled.labels.clone(),
            created_at: created,
            age_minutes: (now - created).num_minutes(),
            witness_run_id: *witness_run,
            witness_job_id: witness.id,
            witness_runner: witness.runner_name.clone(),
            witness_started_at: witness.started_at.unwrap_or(created),
        });
    }
    // One stall per run: its oldest job.
    stalls.sort_by_key(|stall| {
        (
            stall.kind != Kind::MergeQueue,
            stall.created_at,
            stall.job_id,
        )
    });
    let mut seen = Vec::new();
    stalls.retain(|stall| {
        let fresh = !seen.contains(&stall.run_id);
        seen.push(stall.run_id);
        fresh
    });
    detection.stalls = stalls;
    detection
}

/// One cancel attempt, kept so it happens once per run.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(super) struct Receipt {
    /// `OWNER/REPO`.
    pub(super) repo: String,
    #[serde(default)]
    pub(super) kind: Kind,
    pub(super) pr: u64,
    pub(super) run_id: u64,
    pub(super) job_id: u64,
    /// The group head (merge queue) or the pull request head.
    #[serde(alias = "group_sha")]
    pub(super) head_sha: String,
    /// RFC 3339.
    pub(super) at: String,
    /// `cancelled` or `failed: ...`.
    pub(super) outcome: String,
    /// `recovered: ...` or `unrecovered: ...`; absent until proved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) progress: Option<String>,
    /// When the progress was proved, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) proved_at: Option<String>,
}

fn parse_at(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

impl Receipt {
    fn at(&self) -> Option<DateTime<Utc>> {
        parse_at(&self.at)
    }

    fn unrecovered(&self) -> bool {
        self.progress
            .as_deref()
            .is_some_and(|progress| progress.starts_with("unrecovered"))
    }

    /// `#<pr>:run:<id>`.
    fn token(&self) -> String {
        format!("#{}:run:{}", self.pr, self.run_id)
    }
}

/// The ledger key for one run.
pub(super) fn receipt_key(repo: &str, run_id: u64) -> String {
    format!("{}#{run_id}", repo.to_ascii_lowercase())
}

/// A pull request as a fresh read sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Entry {
    /// The pull request head.
    pub(super) head: Option<String>,
    /// The queue position, when queued.
    pub(super) position: Option<u64>,
    /// The merge group head, when queued.
    pub(super) group: Option<String>,
}

/// A run as a fresh read sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RunState {
    pub(super) status: String,
    pub(super) conclusion: Option<String>,
    pub(super) run_attempt: u64,
}

/// The acts and reads the recovery needs, kept apart so it is testable.
pub(super) trait Acts {
    /// Re-read the pull request and its queue entry.
    fn reread_entry(&self, pr: u64) -> Result<Entry, String>;
    /// Re-read every job of the run's latest attempt, proved complete.
    fn reread_jobs(&self, run_id: u64) -> Result<Vec<Job>, String>;
    /// Cancel the run under the run mutation guard; `Ok` once GitHub accepted.
    fn cancel(&self, run: &StewardRun) -> Result<(), String>;
    /// Save the steward ledger now.
    fn persist(&self, ledger: &StewardLedger) -> Result<(), String>;
    /// Read one run.
    fn read_run(&self, run_id: u64) -> Result<RunState, String>;
}

/// What the act did for one stall this pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct Action {
    pub(super) kind: Kind,
    pub(super) pr: u64,
    pub(super) run_id: u64,
    /// `would_cancel`, `cancelled`, `cancel_failed`, `refused`, `deferred` or
    /// `already_attempted`.
    pub(super) outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) detail: Option<String>,
}

/// Re-read the entry and the job right before a cancel. `Err` names why the
/// cancel is refused.
fn still_stalled(stall: &Stall, acts: &dyn Acts) -> Result<(), String> {
    let entry = acts
        .reread_entry(stall.pr)
        .map_err(|error| format!("entry re-read failed: {error}"))?;
    match stall.kind {
        Kind::MergeQueue => {
            if entry.position != stall.position
                || !entry
                    .group
                    .as_deref()
                    .is_some_and(|group| group.eq_ignore_ascii_case(&stall.head_sha))
            {
                return Err(format!(
                    "entry moved: position {:?}, group {}",
                    entry.position,
                    entry.group.as_deref().unwrap_or("absent")
                ));
            }
        }
        Kind::PullRequest => {
            if !entry
                .head
                .as_deref()
                .is_some_and(|head| head.eq_ignore_ascii_case(&stall.head_sha))
                || entry.position.is_some()
            {
                return Err(format!(
                    "pull request moved: head {}, queued {}",
                    entry.head.as_deref().unwrap_or("unknown"),
                    entry.position.is_some()
                ));
            }
        }
    }
    // Every job of the run, as detection judged them: a sibling that started
    // since would be cancelled with the run.
    let jobs = acts
        .reread_jobs(stall.run_id)
        .map_err(|error| format!("job re-read failed: {error}"))?;
    if let Some(started) = jobs.iter().find(|job| {
        job.id != stall.job_id
            && !job.status.eq_ignore_ascii_case("completed")
            && (job.status.eq_ignore_ascii_case("in_progress") || job.has_runner())
    }) {
        return Err(format!(
            "job {} of the run started ({}) since detection",
            started.id, started.status
        ));
    }
    let job = jobs
        .into_iter()
        .find(|job| job.id == stall.job_id)
        .ok_or_else(|| format!("job {} is no longer in the run", stall.job_id))?;
    if !job.waiting() {
        return Err(format!(
            "job {} is now {} {}",
            stall.job_id,
            job.status,
            if job.has_runner() {
                "with a runner"
            } else {
                "with no runner"
            }
        ));
    }
    Ok(())
}

/// Decide and, in `apply` on an applying pass, cancel at most one stalled
/// run. `opted_out` names pull requests the steward must not touch.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // The stalls, their policy, the pass and its one actor, each refusal in order.
pub(super) fn act(
    detection: &Detection,
    policy: &MqStallPolicy,
    applying: bool,
    opted_out: &std::collections::BTreeSet<u64>,
    observation: &RepoObservation,
    ledger: &mut StewardLedger,
    acts: &dyn Acts,
    now: DateTime<Utc>,
) -> Vec<Action> {
    let mut actions = Vec::new();
    if detection.unread.is_some() {
        return actions;
    }
    let prefix = format!("{}#", observation.repo.to_ascii_lowercase());
    let hour = now - chrono::Duration::hours(1);
    let mut cancelled_this_pass = false;
    for stall in &detection.stalls {
        let mode = stall.kind.mode(policy);
        if mode == MqStallMode::Off {
            continue;
        }
        let mut record = |outcome: &str, detail: Option<String>| {
            actions.push(Action {
                kind: stall.kind,
                pr: stall.pr,
                run_id: stall.run_id,
                outcome: outcome.to_owned(),
                detail,
            });
        };
        let key = receipt_key(&observation.repo, stall.run_id);
        if let Some(done) = ledger.mq_stall_cancels.get(&key) {
            record(
                "already_attempted",
                Some(format!(
                    "{}; {}",
                    done.outcome,
                    done.progress.as_deref().unwrap_or("progress pending")
                )),
            );
            continue;
        }
        if opted_out.contains(&stall.pr) {
            record(
                "refused",
                Some(format!("#{} carries the opt-out label", stall.pr)),
            );
            continue;
        }
        // Pull request stalls are report only, whatever the mode says.
        if mode == MqStallMode::Shadow || !applying || stall.kind == Kind::PullRequest {
            record("would_cancel", Some(stall.token()));
            continue;
        }
        if cancelled_this_pass {
            record(
                "deferred",
                Some("one stall cancel per repository per pass".to_owned()),
            );
            continue;
        }
        let recent = ledger
            .mq_stall_cancels
            .iter()
            .filter(|(key, receipt)| {
                key.starts_with(&prefix) && receipt.at().is_some_and(|at| at > hour)
            })
            .count();
        let per_pr = now - chrono::Duration::seconds(PER_PR_WINDOW_SECS);
        if let Some(earlier) = ledger.mq_stall_cancels.iter().find(|(key, receipt)| {
            key.starts_with(&prefix)
                && receipt.pr == stall.pr
                && receipt.at().is_some_and(|at| at > per_pr)
        }) {
            record(
                "refused",
                Some(format!(
                    "#{} already had a stall cancel at {} (run {}); one per pull request in {} hours",
                    stall.pr,
                    earlier.1.at,
                    earlier.1.run_id,
                    PER_PR_WINDOW_SECS / 3600
                )),
            );
            continue;
        }
        if recent >= policy.max_cancels_per_hour as usize {
            record(
                "refused",
                Some(format!(
                    "{recent} stall cancels in the last hour (cap {})",
                    policy.max_cancels_per_hour
                )),
            );
            continue;
        }
        let Some(run) = observation.runs.iter().find(|run| run.id == stall.run_id) else {
            record("refused", Some("run not in this pass's census".to_owned()));
            continue;
        };
        if let Err(why) = still_stalled(stall, acts) {
            record("refused", Some(why));
            continue;
        }
        cancelled_this_pass = true;
        // The intent is durable before the cancel: a crash or a failed save
        // after it can never lose the attempt from the limits or the proof.
        ledger.mq_stall_cancels.insert(
            key.clone(),
            Receipt {
                repo: observation.repo.clone(),
                kind: stall.kind,
                pr: stall.pr,
                run_id: stall.run_id,
                job_id: stall.job_id,
                head_sha: stall.head_sha.clone(),
                at: now.to_rfc3339(),
                outcome: INTENT.to_owned(),
                progress: None,
                proved_at: None,
            },
        );
        if let Err(error) = acts.persist(ledger) {
            ledger.mq_stall_cancels.remove(&key);
            record(
                "intent_not_saved",
                Some(format!("{}; not cancelled: {error}", stall.token())),
            );
            continue;
        }
        let (outcome, result) = match acts.cancel(run) {
            Ok(()) => ("cancelled".to_owned(), "cancelled"),
            Err(error) => (format!("failed: {error}"), "cancel_failed"),
        };
        if let Some(receipt) = ledger.mq_stall_cancels.get_mut(&key) {
            receipt.outcome.clone_from(&outcome);
        }
        let saved = acts
            .persist(ledger)
            .err()
            .map_or_else(String::new, |error| {
                format!("; outcome not saved ({error}), the next pass reads {CANCEL_UNKNOWN}")
            });
        record(result, Some(format!("{}; {outcome}{saved}", stall.token())));
    }
    actions
}

/// How a cancelled queue-head run's pull request moved, if it did.
fn queue_moved(
    receipt: &Receipt,
    observation: &RepoObservation,
    reports: &[PrReport],
    cancelled_at: DateTime<Utc>,
) -> Option<String> {
    let Some(pr) = observation
        .prs
        .iter()
        .find(|pr| pr.fact.number == receipt.pr)
    else {
        return Some("pull request no longer open".to_owned());
    };
    if pr.fact.queue_position.is_some()
        && let Some(group) = observation.merge_group_heads.get(&receipt.pr)
        && !group.eq_ignore_ascii_case(&receipt.head_sha)
    {
        return Some(format!(
            "re-enqueued on group {}",
            group.get(..12).unwrap_or(group)
        ));
    }
    if let Some(run) = observation.runs.iter().find(|run| {
        run.id != receipt.run_id
            && super::merge_group_pr_number(run) == Some(receipt.pr)
            && run_created(run).is_some_and(|at| at >= cancelled_at)
    }) {
        return Some(format!("fresh merge_group run {}", run.id));
    }
    reports
        .iter()
        .find(|report| {
            report.number == receipt.pr
                && report.mutation.as_deref().is_some_and(|mutation| {
                    mutation
                        .split(',')
                        .any(|part| part.contains("enqueued") && !part.starts_with("would_"))
                })
        })
        .map(|_| "re-enqueued this pass".to_owned())
}

/// One receipt's next step from a fresh run read: `Some(verdict)` once
/// proved, `None` while it waits.
fn prove(
    receipt: &Receipt,
    state: &RunState,
    observation: &RepoObservation,
    reports: &[PrReport],
    at: DateTime<Utc>,
    now: DateTime<Utc>,
    lines: &mut Vec<String>,
) -> Option<String> {
    let cancelled = state.status.eq_ignore_ascii_case("completed")
        && state.conclusion.as_deref() == Some("cancelled");
    // Ok: recovered. Err: not yet, with why.
    let step: Result<String, String> = match receipt.kind {
        Kind::MergeQueue if !cancelled => Err(format!(
            "run is {}/{} after `{}`",
            state.status,
            state.conclusion.as_deref().unwrap_or("none"),
            receipt.outcome
        )),
        Kind::MergeQueue => queue_moved(receipt, observation, reports, at).ok_or_else(|| {
            format!(
                "run cancelled, but #{} is neither re-enqueued nor on a fresh merge_group run",
                receipt.pr
            )
        }),
        // No pull request stall is ever cancelled; a receipt of one is from
        // an older binary and is closed.
        Kind::PullRequest => {
            return Some("unrecovered: pull request stalls are report only".to_owned());
        }
    };
    match step {
        Ok(how) => Some(format!("recovered: {how}")),
        Err(why) if (now - at).num_seconds() >= RECOVERY_WINDOW_SECS => {
            Some(format!("unrecovered: {why}"))
        }
        Err(why) => {
            lines.push(format!("{}: progress pending: {why}", receipt.token()));
            None
        }
    }
}

/// Prove each pending receipt of `observation.repo` from a fresh run read.
/// Only an applying pass calls it.
/// Returns (kind, token) of receipts that became unrecovered, and one line
/// per receipt touched.
pub(super) fn progress(
    acts: &dyn Acts,
    observation: &RepoObservation,
    reports: &[PrReport],
    ledger: &mut StewardLedger,
    now: DateTime<Utc>,
) -> (Vec<(Kind, String)>, Vec<String>) {
    let prefix = format!("{}#", observation.repo.to_ascii_lowercase());
    let mut unrecovered = Vec::new();
    let mut lines = Vec::new();
    for (key, receipt) in &mut ledger.mq_stall_cancels {
        if !key.starts_with(&prefix) || receipt.progress.is_some() {
            continue;
        }
        // An earlier pass saved the intent and never its outcome.
        if receipt.outcome == INTENT {
            CANCEL_UNKNOWN.clone_into(&mut receipt.outcome);
            lines.push(format!(
                "{}: the cancel's outcome was never saved; read as {CANCEL_UNKNOWN}",
                receipt.token()
            ));
        }
        let Some(at) = receipt.at() else {
            receipt.progress = Some("unrecovered: receipt time unreadable".to_owned());
            receipt.proved_at = Some(now.to_rfc3339());
            unrecovered.push((receipt.kind, receipt.token()));
            continue;
        };
        let last = at;
        if (now - last).num_seconds() < PROGRESS_MIN_SECS {
            continue;
        }
        let verdict = match acts.read_run(receipt.run_id) {
            Err(error) if (now - last).num_seconds() >= PROGRESS_UNREAD_SECS => {
                Some(format!("unrecovered: run unread for an hour: {error}"))
            }
            Err(error) => {
                lines.push(format!(
                    "{}: progress pending, run unread: {error}",
                    receipt.token()
                ));
                None
            }
            Ok(state) => prove(receipt, &state, observation, reports, at, now, &mut lines),
        };
        if let Some(verdict) = verdict {
            lines.push(format!("{}: {verdict}", receipt.token()));
            receipt.progress = Some(verdict);
            receipt.proved_at = Some(now.to_rfc3339());
            if receipt.unrecovered() {
                unrecovered.push((receipt.kind, receipt.token()));
            }
        }
    }
    let horizon = now - chrono::Duration::days(RECEIPT_RETENTION_DAYS);
    ledger.mq_stall_cancels.retain(|_, receipt| {
        receipt.progress.is_none() || receipt.at().is_some_and(|at| at > horizon)
    });
    (unrecovered, lines)
}

/// The summary tokens the tick prints.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(super) struct Summary {
    /// Queue head stalls: `#<pr>:run:<id>:job:<id>:<age>m`.
    pub(super) mq_stall: Vec<String>,
    /// Pull request stalls, the same shape.
    pub(super) pr_stall: Vec<String>,
    /// `<outcome>:#<pr>:run:<id>` per act.
    pub(super) act: Vec<String>,
    /// Queue head receipts that are unrecovered: `#<pr>:run:<id>`.
    pub(super) mq_stall_unrecovered: Vec<String>,
    /// Pull request receipts that are unrecovered.
    pub(super) pr_stall_unrecovered: Vec<String>,
    /// What failed: `unread` when detection could not read, and
    /// `<outcome>:#<pr>:run:<id>` for a cancel that failed or was refused for
    /// an unsaved intent.
    pub(super) errors: Vec<String>,
}

/// Outcomes that are failures of the recovery itself.
const FAILED_OUTCOMES: [&str; 2] = ["cancel_failed", "intent_not_saved"];

/// The repository report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) struct RepoStatus {
    pub(super) mode: MqStallMode,
    pub(super) pr_mode: MqStallMode,
    pub(super) threshold_minutes: u32,
    pub(super) max_cancels_per_hour: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) config_error: Option<String>,
    pub(super) detection: Detection,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) actions: Vec<Action>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) progress: Vec<String>,
    pub(super) summary: Summary,
}

/// Build the report and its summary tokens.
pub(super) fn status(
    policy: &MqStallPolicy,
    detection: Detection,
    actions: Vec<Action>,
    progress_lines: Vec<String>,
    newly_unrecovered: Vec<(Kind, String)>,
    ledger: &StewardLedger,
    repo: &str,
) -> RepoStatus {
    let mut summary = Summary::default();
    let mut unrecovered = newly_unrecovered;
    for stall in &detection.stalls {
        match stall.kind {
            Kind::MergeQueue => summary.mq_stall.push(stall.token()),
            Kind::PullRequest => summary.pr_stall.push(stall.token()),
        }
        // A stall still standing on a run whose cancel did not recover it.
        if let Some(receipt) = ledger
            .mq_stall_cancels
            .get(&receipt_key(repo, stall.run_id))
            .filter(|receipt| receipt.unrecovered())
        {
            unrecovered.push((receipt.kind, receipt.token()));
        }
    }
    for action in &actions {
        let token = format!("{}:#{}:run:{}", action.outcome, action.pr, action.run_id);
        if FAILED_OUTCOMES.contains(&action.outcome.as_str()) {
            summary.errors.push(token.clone());
        }
        summary.act.push(token);
    }
    if detection.unread.is_some() {
        summary.errors.push("unread".to_owned());
    }
    for (kind, token) in unrecovered {
        let list = match kind {
            Kind::MergeQueue => &mut summary.mq_stall_unrecovered,
            Kind::PullRequest => &mut summary.pr_stall_unrecovered,
        };
        if !list.contains(&token) {
            list.push(token);
        }
    }
    RepoStatus {
        mode: policy.mode,
        pr_mode: policy.pr_mode,
        threshold_minutes: policy.minutes,
        max_cancels_per_hour: policy.max_cancels_per_hour,
        config_error: policy.error.clone(),
        detection,
        actions,
        progress: progress_lines,
        summary,
    }
}

impl RepoStatus {
    /// The repository errors this step adds to the pass: a detection that
    /// could not read, and a cancel that failed or never saved its intent.
    pub(super) fn errors(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if let Some(unread) = &self.detection.unread {
            errors.push(format!(
                "mq_stall: detection unread, nothing decided: {unread}"
            ));
        }
        for action in &self.actions {
            if FAILED_OUTCOMES.contains(&action.outcome.as_str()) {
                errors.push(format!(
                    "mq_stall: {} #{} run {}: {}",
                    action.outcome,
                    action.pr,
                    action.run_id,
                    action.detail.as_deref().unwrap_or_default()
                ));
            }
        }
        errors
    }
}

/// No acts: a pass without mutation control decides, never acts.
pub(super) struct NoActs;

impl Acts for NoActs {
    fn reread_entry(&self, _pr: u64) -> Result<Entry, String> {
        Err("no mutation control".to_owned())
    }
    fn reread_jobs(&self, _run_id: u64) -> Result<Vec<Job>, String> {
        Err("no mutation control".to_owned())
    }
    fn cancel(&self, _run: &StewardRun) -> Result<(), String> {
        Err("no mutation control".to_owned())
    }
    fn persist(&self, _ledger: &StewardLedger) -> Result<(), String> {
        Err("no mutation control".to_owned())
    }
    fn read_run(&self, _run_id: u64) -> Result<RunState, String> {
        Err("no mutation control".to_owned())
    }
}

/// The steward's GitHub acts and reads.
pub(super) struct GitHubActs<'a> {
    pub(super) actions: &'a crate::cloud::GitHubActions,
    pub(super) control: Option<&'a MutationControl>,
    pub(super) observation: &'a RepoObservation,
    /// Where the steward ledger is saved.
    pub(super) ledger_path: &'a std::path::Path,
}

impl GitHubActs<'_> {
    fn api(&self, path: &str) -> Result<Value, String> {
        read_value(
            &|argv: &[String]| self.actions.run_gh(argv).map_err(|error| error.to_string()),
            path,
        )
    }

    /// Run one guarded run mutation.
    fn guarded(
        &self,
        run: &StewardRun,
        action: &str,
        tokens: (&str, &str),
        reason: &str,
        mutate: impl FnOnce() -> Result<(), String>,
    ) -> Result<(), String> {
        let control = self
            .control
            .ok_or_else(|| "a run mutation requires mutation control".to_owned())?;
        let guard = super::acquire_run_mutation_guard(control, self.observation, run, action)?;
        let result = mutate();
        let (token, reason) = match &result {
            Ok(()) => (tokens.0, reason.to_owned()),
            Err(error) => (tokens.1, error.clone()),
        };
        guard
            .finish_with_reason(token, &reason)
            .map_err(|audit| format!("mutation audit failed: {audit}"))?;
        result
    }
}

impl Acts for GitHubActs<'_> {
    fn reread_entry(&self, pr: u64) -> Result<Entry, String> {
        let repo = &self.observation.repo;
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| format!("repository `{repo}` is not OWNER/REPO"))?;
        let query = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid mergeQueueEntry{position headCommit{oid}}}}}";
        let raw = self
            .actions
            .clone()
            .with_repo_override(repo)
            .run_gh(&[
                "api".to_owned(),
                "graphql".to_owned(),
                "-f".to_owned(),
                format!("query={query}"),
                "-f".to_owned(),
                format!("owner={owner}"),
                "-f".to_owned(),
                format!("name={name}"),
                "-F".to_owned(),
                format!("number={pr}"),
            ])
            .map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
        let node = value
            .pointer("/data/repository/pullRequest")
            .filter(|node| !node.is_null())
            .ok_or_else(|| "pull request not in the answer".to_owned())?;
        let entry = node.get("mergeQueueEntry").filter(|entry| !entry.is_null());
        Ok(Entry {
            head: node
                .get("headRefOid")
                .and_then(Value::as_str)
                .map(str::to_owned),
            position: entry
                .and_then(|entry| entry.get("position"))
                .and_then(Value::as_u64),
            group: entry
                .and_then(|entry| entry.pointer("/headCommit/oid"))
                .and_then(Value::as_str)
                .map(str::to_owned),
        })
    }

    fn persist(&self, ledger: &StewardLedger) -> Result<(), String> {
        super::ledger::save_ledger(self.ledger_path, ledger)
            .map_err(|error| error.message().to_owned())
    }

    fn reread_jobs(&self, run_id: u64) -> Result<Vec<Job>, String> {
        read_jobs(
            &|argv: &[String]| self.actions.run_gh(argv).map_err(|error| error.to_string()),
            &self.observation.repo,
            run_id,
        )
    }

    fn cancel(&self, run: &StewardRun) -> Result<(), String> {
        self.guarded(
            run,
            "runner steward cancel a run whose job GitHub never gave a runner",
            ("mq_stall_cancelled", "mq_stall_cancel_failed"),
            "job queued with no runner while newer jobs with its labels started",
            || {
                self.actions
                    .cancel_workflow_run(&self.observation.repo, run.id)
                    .map_err(|error| error.to_string())
            },
        )
    }

    fn read_run(&self, run_id: u64) -> Result<RunState, String> {
        let value = self.api(&format!(
            "repos/{}/actions/runs/{run_id}",
            self.observation.repo
        ))?;
        Ok(RunState {
            status: value
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("run {run_id} answer had no status"))?
                .to_owned(),
            conclusion: value
                .get("conclusion")
                .and_then(Value::as_str)
                .map(str::to_owned),
            run_attempt: value
                .get("run_attempt")
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("run {run_id} answer had no run_attempt"))?,
        })
    }
}
