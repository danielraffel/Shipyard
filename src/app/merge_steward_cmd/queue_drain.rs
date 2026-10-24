//! `shipyard queue triage|prioritize|resume`: the GitHub adapter for
//! [`crate::queue_drain`].
//!
//! Reads go through the configured GitHub App token path (`GitHubActions`),
//! never ambient personal auth. Every cancellation and re-run holds the same
//! merge-queue mutation guard as the steward, so `shipyard merge-queue hold`
//! and the configured mutation machine apply here too. Each cancellation is
//! written to the restore ledger before GitHub is asked to cancel, so a crash
//! can never lose a run that `resume` should bring back.

use super::observation::{
    OPEN_PR_LIST_FIELDS, active_runs, canonical_repo_name, complete_checks_for_head, gh_json,
    merge_queue_snapshot, parse_pr, required_checks,
};
use super::{
    BTreeMap, BTreeSet, CliFailure, ExitCode, GitHubActions, MergeQueueMutationGuard, OpenOptions,
    Path, PathBuf, RequiredCheck, RuntimeMode, RuntimePaths, ShipState, ShipStateStore, StewardRun,
    Utc, Value, Write, acquire_ledger_lock, fs, is_full_sha, json, write_json_envelope,
};
use crate::queue_drain::{
    CancelCause, DrainJob, DrainPullRequest, DrainRun, PrioritizePlan, RestoreEntry, RestoreLedger,
    ResumeDecision, RunTriage, RunVerdict, UnblockCandidate, job_is_waiting, plan_prioritize,
    plan_triage, red_required_contexts, resume_decision, unblocking_candidates,
};

const MAX_OPEN_PR_ROWS: usize = 1000;
const LEDGER_FILE: &str = "queue-drain.json";

/// Shared target for every queue-drain subcommand.
pub(in crate::app) struct QueueDrainArgs {
    pub(in crate::app) repo: Option<String>,
    pub(in crate::app) base: String,
    pub(in crate::app) apply: bool,
    pub(in crate::app) ledger: Option<PathBuf>,
}

/// Runtime context shared by the three entry points.
pub(in crate::app) struct QueueDrainRuntime<'a> {
    pub(in crate::app) cwd: &'a Path,
    pub(in crate::app) mode: RuntimeMode,
    pub(in crate::app) runtime_paths: &'a RuntimePaths,
    pub(in crate::app) actions: &'a GitHubActions,
}

struct Observation {
    repo: String,
    base: String,
    required: Vec<RequiredCheck>,
    prs: Vec<DrainPullRequest>,
    runs: Vec<StewardRun>,
}

#[derive(serde::Serialize)]
struct MutationResult {
    run_id: u64,
    pr_number: Option<u64>,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn ledger_path(args: &QueueDrainArgs, runtime: &QueueDrainRuntime<'_>) -> PathBuf {
    args.ledger
        .clone()
        .unwrap_or_else(|| runtime.runtime_paths.state_dir.join(LEDGER_FILE))
}

fn scoped_actions(actions: &GitHubActions, repo: &str) -> GitHubActions {
    actions.clone().with_repo_override(repo)
}

fn resolve_repo(repo: Option<String>, cwd: &Path) -> Result<String, CliFailure> {
    let mut repos = super::observation::resolve_repos(repo.into_iter().collect(), cwd)?;
    Ok(repos.remove(0))
}

fn observe(actions: &GitHubActions, repo: &str, base: &str) -> Result<Observation, String> {
    let settings = gh_json(
        actions,
        &["api".to_owned(), format!("repos/{repo}")],
        "repository settings",
    )?;
    let repo = canonical_repo_name(&settings)?;
    let required = required_checks(actions, &repo, base)?;
    let mut prs = open_pull_requests(actions, &repo, base)?;
    // Merge-queue membership is read before runs so a pull request that is
    // enqueued is never mistaken for one whose PR-head CI may be cancelled.
    let (_, positions, _, _) = merge_queue_snapshot(actions, &repo, base)?;
    for pr in &mut prs {
        pr.pr.queue_position = positions.get(&pr.pr.number).copied();
    }
    let runs = active_runs(actions, &repo)?;
    Ok(Observation {
        repo,
        base: base.to_owned(),
        required,
        prs,
        runs,
    })
}

fn open_pull_requests(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
) -> Result<Vec<DrainPullRequest>, String> {
    let value = gh_json(
        actions,
        &[
            "pr".to_owned(),
            "list".to_owned(),
            "--repo".to_owned(),
            repo.to_owned(),
            "--state".to_owned(),
            "open".to_owned(),
            "--base".to_owned(),
            base.to_owned(),
            "--limit".to_owned(),
            MAX_OPEN_PR_ROWS.to_string(),
            "--json".to_owned(),
            // `body` stays out of the bulk read (see OPEN_PR_LIST_FIELDS); it
            // is fetched over REST only when the base is red.
            format!("{OPEN_PR_LIST_FIELDS},title"),
        ],
        "open PR list",
    )?;
    let rows = value
        .as_array()
        .ok_or_else(|| "open PR list was not an array".to_owned())?;
    if rows.len() == MAX_OPEN_PR_ROWS {
        return Err(format!(
            "open PR list reached {MAX_OPEN_PR_ROWS}; refusing a possibly partial snapshot"
        ));
    }
    rows.iter()
        .map(|row| {
            let observed = parse_pr(row, &BTreeMap::new())?;
            let mut pr = observed.fact;
            // Checks are hydrated per head from REST, never from a rollup that
            // may be partial.
            pr.checks.clear();
            Ok(DrainPullRequest {
                pr,
                title: row
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                body: String::new(),
            })
        })
        .collect()
}

/// Fill in complete current-head checks for the named pull requests.
fn hydrate_checks(
    actions: &GitHubActions,
    observation: &mut Observation,
    numbers: &BTreeSet<u64>,
) -> Result<(), String> {
    for pr in &mut observation.prs {
        if numbers.contains(&pr.pr.number) && is_full_sha(&pr.pr.head_sha) {
            pr.pr.checks = complete_checks_for_head(actions, &observation.repo, &pr.pr.head_sha)?;
        }
    }
    Ok(())
}

/// Pull requests whose doom can depend on their checks: those owning an active
/// pull-request run at their current head.
fn prs_needing_checks(observation: &Observation) -> BTreeSet<u64> {
    observation
        .runs
        .iter()
        .filter(|run| crate::queue_drain::is_pull_request_run(run))
        .filter_map(|run| {
            crate::queue_drain::pull_request_for_run(run, &observation.prs)
                .filter(|pr| pr.pr.head_sha.eq_ignore_ascii_case(&run.head_sha))
                .map(|pr| pr.pr.number)
        })
        .collect()
}

fn hydrate_bodies(actions: &GitHubActions, observation: &mut Observation) -> Result<(), String> {
    let mut bodies = BTreeMap::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{}/pulls?state=open&base={}&per_page=100&page={page}",
                    observation.repo,
                    super::observation::encode_path_segment(&observation.base)
                ),
            ],
            "open PR descriptions",
        )?;
        let rows = value
            .as_array()
            .ok_or_else(|| "open PR descriptions were not an array".to_owned())?;
        for row in rows {
            if let Some(number) = row.get("number").and_then(Value::as_u64) {
                let body = row.get("body").and_then(Value::as_str).unwrap_or_default();
                bodies.insert(number, body.to_owned());
            }
        }
        if rows.len() < 100 {
            break;
        }
    }
    for pr in &mut observation.prs {
        if let Some(body) = bodies.remove(&pr.pr.number) {
            pr.body = body;
        }
    }
    Ok(())
}

fn base_head_sha(actions: &GitHubActions, repo: &str, base: &str) -> Result<String, String> {
    let value = gh_json(
        actions,
        &[
            "api".to_owned(),
            format!(
                "repos/{repo}/commits/{}",
                super::observation::encode_path_segment(base)
            ),
        ],
        "base branch head",
    )?;
    value
        .get("sha")
        .and_then(Value::as_str)
        .filter(|sha| is_full_sha(sha))
        .map(str::to_owned)
        .ok_or_else(|| "base branch head response missing sha".to_owned())
}

fn fetch_jobs(actions: &GitHubActions, repo: &str, run_id: u64) -> Result<Vec<DrainJob>, String> {
    let mut jobs = Vec::new();
    for page in 1..=10 {
        let value = gh_json(
            actions,
            &[
                "api".to_owned(),
                format!(
                    "repos/{repo}/actions/runs/{run_id}/jobs?filter=latest&per_page=100&page={page}"
                ),
            ],
            "workflow jobs",
        )?;
        let rows = value
            .get("jobs")
            .and_then(Value::as_array)
            .ok_or_else(|| format!("workflow run {run_id} response missing jobs"))?;
        for row in rows {
            jobs.push(parse_drain_job(row)?);
        }
        if rows.len() < 100 {
            return Ok(jobs);
        }
    }
    Err(format!(
        "workflow run {run_id} exceeds 1000 jobs; refusing partial scan"
    ))
}

fn parse_drain_job(row: &Value) -> Result<DrainJob, String> {
    let field = |name: &str| {
        row.get(name)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| format!("workflow job missing {name}"))
    };
    Ok(DrainJob {
        id: row
            .get("id")
            .and_then(Value::as_u64)
            .ok_or_else(|| "workflow job missing id".to_owned())?,
        name: field("name")?,
        status: field("status")?,
        labels: row
            .get("labels")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        created_at: field("created_at")?,
    })
}

/// Fetch jobs for the target's runs, then for every other active pull-request
/// run created before the target's newest waiting job. A run created later
/// cannot hold a job that queued ahead.
fn hydrate_drain_runs(
    actions: &GitHubActions,
    observation: &Observation,
    target: u64,
) -> Result<Vec<DrainRun>, String> {
    let owner = |run: &StewardRun| {
        crate::queue_drain::pull_request_for_run(run, &observation.prs).map(|pr| pr.pr.number)
    };
    let candidates = observation
        .runs
        .iter()
        .filter(|run| {
            crate::queue_drain::is_pull_request_run(run)
                && crate::queue_drain::run_is_active(&run.status)
        })
        .collect::<Vec<_>>();
    let mut drains = Vec::new();
    for run in candidates.iter().filter(|run| owner(run) == Some(target)) {
        drains.push(DrainRun {
            run: (*run).clone(),
            jobs: fetch_jobs(actions, &observation.repo, run.id)?,
        });
    }
    let horizon = drains
        .iter()
        .flat_map(|drain| drain.jobs.iter())
        .filter(|job| job_is_waiting(&job.status))
        .map(|job| job.created_at.clone())
        .max();
    let Some(horizon) = horizon else {
        return Ok(drains);
    };
    for run in candidates.iter().filter(|run| owner(run) != Some(target)) {
        if run.created_at.as_str() > horizon.as_str() {
            continue;
        }
        drains.push(DrainRun {
            run: (*run).clone(),
            jobs: fetch_jobs(actions, &observation.repo, run.id)?,
        });
    }
    Ok(drains)
}

fn load_ledger(path: &Path) -> Result<RestoreLedger, CliFailure> {
    match fs::read_to_string(path) {
        Ok(raw) => serde_json::from_str(&raw)
            .map_err(|error| CliFailure::new(1, format!("invalid queue-drain ledger: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RestoreLedger::default()),
        Err(error) => Err(CliFailure::new(
            1,
            format!(
                "could not read queue-drain ledger {}: {error}",
                path.display()
            ),
        )),
    }
}

fn save_ledger(path: &Path, ledger: &RestoreLedger) -> Result<(), String> {
    let _writer_domain = crate::writer_domain_lease::acquire_for_protected_path(path)
        .map_err(|error| error.to_string())?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    }
    let payload = serde_json::to_vec_pretty(ledger)
        .map_err(|error| format!("could not encode queue-drain ledger: {error}"))?;
    let temp = path.with_extension("json.tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temp)
        .map_err(|error| format!("could not open {}: {error}", temp.display()))?;
    file.write_all(&payload)
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("could not write {}: {error}", temp.display()))?;
    crate::queue::replace_file_with_windows_retry(&temp, path)
        .map_err(|error| format!("could not publish {}: {error}", path.display()))
}

fn mutation_guard(
    runtime: &QueueDrainRuntime<'_>,
    observation: &Observation,
    pr_number: u64,
    head_sha: &str,
    action: &str,
) -> Result<MergeQueueMutationGuard, String> {
    let store = ShipStateStore::new(runtime.runtime_paths.state_dir.join("ship"))
        .map_err(|error| format!("could not open merge-queue mutation state: {error}"))?;
    let branch = observation
        .prs
        .iter()
        .find(|pr| pr.pr.number == pr_number)
        .map_or("", |pr| pr.pr.head_branch.as_str());
    let state = ShipState::new(
        pr_number,
        &observation.repo,
        branch,
        &observation.base,
        head_sha,
        "queue-drain",
    );
    MergeQueueMutationGuard::acquire_in_mode(
        &store,
        runtime.cwd,
        runtime.mode,
        &runtime.runtime_paths.global_dir,
        &state,
        action,
    )
}

/// Write-ahead cancel: record, re-check, cancel, and roll the record back when
/// GitHub refuses. `still_valid` re-reads live state right before the cancel.
fn cancel_and_record(
    runtime: &QueueDrainRuntime<'_>,
    actions: &GitHubActions,
    observation: &Observation,
    ledger_path: &Path,
    ledger: &mut RestoreLedger,
    entry: RestoreEntry,
    still_valid: &dyn Fn() -> Result<Option<String>, String>,
) -> MutationResult {
    let run_id = entry.run_id;
    let pr_number = Some(entry.pr_number);
    let result = |outcome: &str, error: Option<String>| MutationResult {
        run_id,
        pr_number,
        outcome: outcome.to_owned(),
        error,
    };
    let guard = match mutation_guard(
        runtime,
        observation,
        entry.pr_number,
        &entry.head_sha,
        &format!("queue drain cancel run {run_id}"),
    ) {
        Ok(guard) => guard,
        Err(error) => return result("refused_by_mutation_guard", Some(error)),
    };
    match enqueued_now(actions, observation, entry.pr_number) {
        Ok(false) => {}
        Ok(true) => {
            let audit = guard.finish("skipped_pull_request_enqueued").err();
            return result("skipped_pull_request_enqueued", audit);
        }
        Err(error) => {
            let _ = guard.finish("live_recheck_failed");
            return result("live_recheck_failed", Some(error));
        }
    }
    match still_valid() {
        Ok(None) => {}
        Ok(Some(skip)) => {
            let audit = guard.finish("skipped_after_live_recheck").err();
            return result(&skip, audit);
        }
        Err(error) => {
            let _ = guard.finish("live_recheck_failed");
            return result("live_recheck_failed", Some(error));
        }
    }
    ledger.record(entry);
    if let Err(error) = save_ledger(ledger_path, ledger) {
        ledger.remove(&observation.repo, run_id);
        let _ = guard.finish("ledger_write_failed");
        return result("ledger_write_failed", Some(error));
    }
    match actions.cancel_workflow_run(&observation.repo, run_id) {
        Ok(()) => result("cancelled", guard.finish("cancel_accepted").err()),
        Err(error) => {
            ledger.remove(&observation.repo, run_id);
            let rollback = save_ledger(ledger_path, ledger).err();
            let _ = guard.finish("cancel_rejected");
            result(
                "cancel_failed",
                Some(format!(
                    "{error}{}",
                    rollback.map_or_else(String::new, |error| format!(
                        "; ledger rollback also failed: {error}"
                    ))
                )),
            )
        }
    }
}

/// Live merge-queue membership, re-read right before every mutation.
fn enqueued_now(
    actions: &GitHubActions,
    observation: &Observation,
    pr_number: u64,
) -> Result<bool, String> {
    let (_, positions, _, _) = merge_queue_snapshot(actions, &observation.repo, &observation.base)?;
    Ok(positions.contains_key(&pr_number))
}

fn run_still_active(
    actions: &GitHubActions,
    repo: &str,
    run_id: u64,
) -> Result<Option<String>, String> {
    let (status, _) = actions
        .run_status_conclusion(repo, run_id)
        .map_err(|error| error.to_string())?;
    Ok((!crate::queue_drain::run_is_active(&status)).then(|| format!("skipped_run_{status}")))
}

/// Cancel yields from a priority plan. Each run's jobs are re-read right before
/// the cancel so a job that started meanwhile keeps the run alive.
fn apply_prioritize(
    runtime: &QueueDrainRuntime<'_>,
    actions: &GitHubActions,
    observation: &Observation,
    ledger_path: &Path,
    ledger: &mut RestoreLedger,
    plan: &PrioritizePlan,
) -> Vec<MutationResult> {
    let mut results = Vec::new();
    for yielded in &plan.yields {
        let Some(pr_number) = yielded.pr_number else {
            results.push(MutationResult {
                run_id: yielded.run_id,
                pr_number: None,
                outcome: "skipped_unattributed".to_owned(),
                error: None,
            });
            continue;
        };
        let status = observation
            .runs
            .iter()
            .find(|run| run.id == yielded.run_id)
            .map_or_else(String::new, |run| run.status.clone());
        let entry = RestoreEntry {
            repo: observation.repo.clone(),
            base: observation.base.clone(),
            run_id: yielded.run_id,
            workflow: yielded.workflow.clone(),
            pr_number,
            head_sha: yielded.head_sha.clone(),
            status_at_cancel: status,
            cause: CancelCause::YieldedTo {
                pr_number: plan.pr_number,
            },
            recorded_at: Utc::now().to_rfc3339(),
        };
        let recheck = || -> Result<Option<String>, String> {
            let jobs = fetch_jobs(actions, &observation.repo, yielded.run_id)?;
            if jobs
                .iter()
                .any(|job| job.status.eq_ignore_ascii_case("in_progress"))
            {
                return Ok(Some("skipped_job_started".to_owned()));
            }
            if !jobs.iter().any(|job| job_is_waiting(&job.status)) {
                return Ok(Some("skipped_nothing_waiting".to_owned()));
            }
            Ok(None)
        };
        results.push(cancel_and_record(
            runtime,
            actions,
            observation,
            ledger_path,
            ledger,
            entry,
            &recheck,
        ));
    }
    results
}

fn emit<W: Write>(
    stdout: &mut W,
    json_output: bool,
    command: &str,
    data: BTreeMap<String, Value>,
    human: &str,
) -> Result<(), CliFailure> {
    if json_output {
        write_json_envelope(stdout, command, data)
            .map_err(|error| CliFailure::new(1, error.to_string()))
    } else {
        stdout
            .write_all(human.as_bytes())
            .map_err(|error| CliFailure::new(1, error.to_string()))
    }
}

fn to_value<T: serde::Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn exit_for(results: &[MutationResult]) -> ExitCode {
    if results.iter().any(|result| result.error.is_some()) {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn human_verdict(triage: &RunTriage) -> String {
    match &triage.verdict {
        RunVerdict::Doomed { reason } => format!("DOOMED {}", to_value(reason)),
        RunVerdict::Viable => "viable".to_owned(),
        RunVerdict::NotPullRequest => "not a pull-request run (never touched)".to_owned(),
        RunVerdict::Unattributed => "no open pull request on this base".to_owned(),
        RunVerdict::InMergeQueue => "pull request is in the merge queue (never touched)".to_owned(),
    }
}

fn human_prioritize(plan: &PrioritizePlan, out: &mut String) {
    use std::fmt::Write as _;
    if let Some(refusal) = &plan.refusal {
        let _ = writeln!(
            out,
            "prioritize #{}: refused {}",
            plan.pr_number,
            to_value(refusal)
        );
        return;
    }
    let _ = writeln!(
        out,
        "prioritize #{}: {} waiting job(s); {} run(s) yield, {} kept (job already running)",
        plan.pr_number,
        plan.target_jobs.len(),
        plan.yields.len(),
        plan.kept.len()
    );
    for yielded in &plan.yields {
        let _ = writeln!(
            out,
            "  yield run {} ({}) PR #{}: {}",
            yielded.run_id,
            yielded.workflow,
            yielded
                .pr_number
                .map_or_else(|| "?".to_owned(), |pr| pr.to_string()),
            yielded.competing_jobs.join(", ")
        );
    }
}

fn human_results(results: &[MutationResult], out: &mut String) {
    use std::fmt::Write as _;
    for result in results {
        let _ = writeln!(
            out,
            "  run {}: {}{}",
            result.run_id,
            result.outcome,
            result
                .error
                .as_ref()
                .map_or_else(String::new, |error| format!(" ({error})"))
        );
    }
}

/// `shipyard queue triage`.
#[allow(clippy::too_many_lines)]
pub(in crate::app) fn queue_triage_command<W: Write>(
    args: &QueueDrainArgs,
    prioritize_unblocking: bool,
    runtime: &QueueDrainRuntime<'_>,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = resolve_repo(args.repo.clone(), runtime.cwd)?;
    let actions = scoped_actions(runtime.actions, &repo);
    let mut observation =
        observe(&actions, &repo, &args.base).map_err(|error| CliFailure::new(1, error))?;
    let needing = prs_needing_checks(&observation);
    hydrate_checks(&actions, &mut observation, &needing)
        .map_err(|error| CliFailure::new(1, error))?;
    let triage = plan_triage(&observation.runs, &observation.prs, &observation.required);

    // Unblocking detection: which required contexts are red on the base, and
    // which open pull request says it fixes one.
    let base_sha = base_head_sha(&actions, &observation.repo, &observation.base)
        .map_err(|error| CliFailure::new(1, error))?;
    let base_checks = complete_checks_for_head(&actions, &observation.repo, &base_sha)
        .map_err(|error| CliFailure::new(1, error))?;
    let red = red_required_contexts(&base_checks, &observation.required);
    if !red.is_empty() {
        hydrate_bodies(&actions, &mut observation).map_err(|error| CliFailure::new(1, error))?;
    }
    let mut candidates = unblocking_candidates(&observation.prs, &red, &observation.required);
    if !candidates.is_empty() {
        // Re-check candidates against their own complete head checks: a pull
        // request whose own required checks already failed fixes nothing.
        let numbers = candidates
            .iter()
            .map(|candidate| candidate.pr_number)
            .collect();
        hydrate_checks(&actions, &mut observation, &numbers)
            .map_err(|error| CliFailure::new(1, error))?;
        candidates = unblocking_candidates(&observation.prs, &red, &observation.required);
    }
    let unblock_target =
        (prioritize_unblocking && candidates.len() == 1).then(|| candidates[0].pr_number);
    let prioritize_plan = match unblock_target {
        Some(target) => {
            let drains = hydrate_drain_runs(&actions, &observation, target)
                .map_err(|error| CliFailure::new(1, error))?;
            Some(plan_prioritize(
                target,
                &drains,
                &observation.prs,
                &observation.required,
            ))
        }
        None => None,
    };

    let ledger_path = ledger_path(args, runtime);
    let mut results = Vec::new();
    let mut prioritize_results = Vec::new();
    if args.apply {
        let _lock = acquire_ledger_lock(&ledger_path)?;
        let mut ledger = load_ledger(&ledger_path)?;
        for entry in &triage {
            let RunVerdict::Doomed { reason } = &entry.verdict else {
                continue;
            };
            let Some(pr_number) = entry.pr_number else {
                continue;
            };
            let record = RestoreEntry {
                repo: observation.repo.clone(),
                base: observation.base.clone(),
                run_id: entry.run_id,
                workflow: entry.workflow.clone(),
                pr_number,
                head_sha: entry.head_sha.clone(),
                status_at_cancel: entry.status.clone(),
                cause: CancelCause::Doomed {
                    reason: reason.clone(),
                },
                recorded_at: Utc::now().to_rfc3339(),
            };
            let run_id = entry.run_id;
            let recheck = || run_still_active(&actions, &observation.repo, run_id);
            results.push(cancel_and_record(
                runtime,
                &actions,
                &observation,
                &ledger_path,
                &mut ledger,
                record,
                &recheck,
            ));
        }
        if let Some(plan) = prioritize_plan
            .as_ref()
            .filter(|plan| plan.refusal.is_none())
        {
            // A run already cancelled as doomed needs no second cancel.
            let mut plan = plan.clone();
            plan.yields
                .retain(|yielded| !results.iter().any(|result| result.run_id == yielded.run_id));
            prioritize_results = apply_prioritize(
                runtime,
                &actions,
                &observation,
                &ledger_path,
                &mut ledger,
                &plan,
            );
        }
    }

    let doomed = triage
        .iter()
        .filter(|entry| matches!(entry.verdict, RunVerdict::Doomed { .. }))
        .count();
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), json!(observation.repo));
    data.insert("base".to_owned(), json!(observation.base));
    data.insert("apply".to_owned(), json!(args.apply));
    data.insert(
        "required_contexts".to_owned(),
        json!(
            observation
                .required
                .iter()
                .map(RequiredCheck::label)
                .collect::<Vec<_>>()
        ),
    );
    data.insert("base_head".to_owned(), json!(base_sha));
    data.insert("red_on_base".to_owned(), json!(red));
    data.insert("runs".to_owned(), to_value(&triage));
    data.insert("doomed".to_owned(), json!(doomed));
    data.insert("unblocking_candidates".to_owned(), to_value(&candidates));
    data.insert("prioritize".to_owned(), to_value(&prioritize_plan));
    data.insert("cancellations".to_owned(), to_value(&results));
    data.insert(
        "prioritize_cancellations".to_owned(),
        to_value(&prioritize_results),
    );
    data.insert(
        "ledger".to_owned(),
        json!(ledger_path.display().to_string()),
    );

    let mut human = String::new();
    {
        use std::fmt::Write as _;
        let _ = writeln!(
            human,
            "{} ({}) active runs: {}, doomed: {}{}",
            observation.repo,
            observation.base,
            triage.len(),
            doomed,
            if args.apply {
                ""
            } else {
                " [dry run; pass --apply]"
            }
        );
        for entry in &triage {
            let _ = writeln!(
                human,
                "  run {} {} [{}] PR {}: {}",
                entry.run_id,
                entry.workflow,
                entry.status,
                entry
                    .pr_number
                    .map_or_else(|| "-".to_owned(), |pr| format!("#{pr}")),
                human_verdict(entry)
            );
        }
        if red.is_empty() {
            let _ = writeln!(human, "base {base_sha}: no required check is red");
        } else {
            let _ = writeln!(
                human,
                "base {base_sha}: red required checks: {}",
                red.join(", ")
            );
        }
        for UnblockCandidate {
            pr_number,
            evidence,
        } in &candidates
        {
            let _ = writeln!(
                human,
                "  unblocking candidate #{pr_number}: {}",
                evidence.join("; ")
            );
        }
        if candidates.len() > 1 {
            let _ = writeln!(
                human,
                "  several candidates; run `shipyard queue prioritize --pr <n>` for the right one"
            );
        }
        if let Some(plan) = &prioritize_plan {
            human_prioritize(plan, &mut human);
        }
        human_results(&results, &mut human);
        human_results(&prioritize_results, &mut human);
    }
    emit(stdout, json_output, "queue-triage", data, &human)?;
    let mut all = results;
    all.extend(prioritize_results);
    Ok(exit_for(&all))
}

/// `shipyard queue prioritize`.
pub(in crate::app) fn queue_prioritize_command<W: Write>(
    args: &QueueDrainArgs,
    pr: u64,
    runtime: &QueueDrainRuntime<'_>,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let repo = resolve_repo(args.repo.clone(), runtime.cwd)?;
    let actions = scoped_actions(runtime.actions, &repo);
    let mut observation =
        observe(&actions, &repo, &args.base).map_err(|error| CliFailure::new(1, error))?;
    hydrate_checks(&actions, &mut observation, &BTreeSet::from([pr]))
        .map_err(|error| CliFailure::new(1, error))?;
    let drains = hydrate_drain_runs(&actions, &observation, pr)
        .map_err(|error| CliFailure::new(1, error))?;
    let plan = plan_prioritize(pr, &drains, &observation.prs, &observation.required);
    let ledger_path = ledger_path(args, runtime);
    let results = if args.apply && plan.refusal.is_none() {
        let _lock = acquire_ledger_lock(&ledger_path)?;
        let mut ledger = load_ledger(&ledger_path)?;
        apply_prioritize(
            runtime,
            &actions,
            &observation,
            &ledger_path,
            &mut ledger,
            &plan,
        )
    } else {
        Vec::new()
    };
    let mut data = BTreeMap::new();
    data.insert("repo".to_owned(), json!(observation.repo));
    data.insert("base".to_owned(), json!(observation.base));
    data.insert("apply".to_owned(), json!(args.apply));
    data.insert("plan".to_owned(), to_value(&plan));
    data.insert("cancellations".to_owned(), to_value(&results));
    data.insert(
        "ledger".to_owned(),
        json!(ledger_path.display().to_string()),
    );
    let mut human = String::new();
    human_prioritize(&plan, &mut human);
    if !args.apply && plan.refusal.is_none() {
        human.push_str("  [dry run; pass --apply]\n");
    }
    human_results(&results, &mut human);
    emit(stdout, json_output, "queue-prioritize", data, &human)?;
    if plan.refusal.is_some() {
        return Ok(ExitCode::from(2));
    }
    Ok(exit_for(&results))
}

#[derive(serde::Serialize)]
struct ResumeReport {
    run_id: u64,
    pr_number: u64,
    #[serde(flatten)]
    decision: ResumeDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// `shipyard queue resume`.
#[allow(clippy::too_many_lines)]
pub(in crate::app) fn queue_resume_command<W: Write>(
    args: &QueueDrainArgs,
    runtime: &QueueDrainRuntime<'_>,
    json_output: bool,
    stdout: &mut W,
) -> Result<ExitCode, CliFailure> {
    let ledger_path = ledger_path(args, runtime);
    let _lock = if args.apply {
        Some(acquire_ledger_lock(&ledger_path)?)
    } else {
        None
    };
    let mut ledger = load_ledger(&ledger_path)?;
    let repos = match &args.repo {
        Some(repo) => vec![repo.clone()],
        None => ledger
            .entries
            .values()
            .map(|entry| entry.repo.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    };
    let mut reports = Vec::new();
    let mut failed = false;
    for repo in repos {
        let actions = scoped_actions(runtime.actions, &repo);
        let entries = ledger
            .for_repo(&repo)
            .into_iter()
            .filter(|entry| entry.base == args.base)
            .cloned()
            .collect::<Vec<_>>();
        if entries.is_empty() {
            continue;
        }
        let mut observation =
            observe(&actions, &repo, &args.base).map_err(|error| CliFailure::new(1, error))?;
        let numbers = entries.iter().map(|entry| entry.pr_number).collect();
        hydrate_checks(&actions, &mut observation, &numbers)
            .map_err(|error| CliFailure::new(1, error))?;
        for entry in entries {
            let decision = resume_decision(&entry, &observation.prs, &observation.required);
            let mut report = ResumeReport {
                run_id: entry.run_id,
                pr_number: entry.pr_number,
                decision: decision.clone(),
                outcome: None,
                error: None,
            };
            if args.apply {
                match decision {
                    ResumeDecision::Keep { .. } | ResumeDecision::HoldForMergeQueue => {}
                    ResumeDecision::Drop { .. } => {
                        ledger.remove(&entry.repo, entry.run_id);
                        report.outcome = Some("cleared".to_owned());
                    }
                    ResumeDecision::Rerun => {
                        let (outcome, error) = rerun_entry(runtime, &actions, &observation, &entry);
                        if error.is_none() && (outcome == "rerun" || outcome.starts_with("cleared"))
                        {
                            ledger.remove(&entry.repo, entry.run_id);
                        }
                        failed |= error.is_some();
                        report.outcome = Some(outcome);
                        report.error = error;
                    }
                }
                if let Err(error) = save_ledger(&ledger_path, &ledger) {
                    return Err(CliFailure::new(1, error));
                }
            }
            reports.push(report);
        }
    }
    let mut data = BTreeMap::new();
    data.insert("base".to_owned(), json!(args.base));
    data.insert("apply".to_owned(), json!(args.apply));
    data.insert("entries".to_owned(), to_value(&reports));
    data.insert(
        "ledger".to_owned(),
        json!(ledger_path.display().to_string()),
    );
    let mut human = String::new();
    {
        use std::fmt::Write as _;
        if reports.is_empty() {
            human.push_str("restore ledger has nothing to resume\n");
        }
        for report in &reports {
            let _ = writeln!(
                human,
                "  run {} PR #{}: {}{}{}",
                report.run_id,
                report.pr_number,
                to_value(&report.decision),
                report
                    .outcome
                    .as_ref()
                    .map_or_else(String::new, |outcome| format!(" -> {outcome}")),
                report
                    .error
                    .as_ref()
                    .map_or_else(String::new, |error| format!(" ({error})"))
            );
        }
        if !args.apply && !reports.is_empty() {
            human.push_str("  [dry run; pass --apply]\n");
        }
    }
    emit(stdout, json_output, "queue-resume", data, &human)?;
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// Re-run the cancelled jobs of one entry. Only a run GitHub reports as
/// completed-and-cancelled is re-run; anything else already ran its course.
fn rerun_entry(
    runtime: &QueueDrainRuntime<'_>,
    actions: &GitHubActions,
    observation: &Observation,
    entry: &RestoreEntry,
) -> (String, Option<String>) {
    let (status, conclusion) = match actions.run_status_conclusion(&observation.repo, entry.run_id)
    {
        Ok(state) => state,
        Err(error) => return ("status_read_failed".to_owned(), Some(error.to_string())),
    };
    if !status.eq_ignore_ascii_case("completed") || !conclusion.eq_ignore_ascii_case("cancelled") {
        // The cancel never landed or the run was re-run elsewhere; nothing to
        // restore, so the entry is cleared.
        return (format!("cleared_run_is_{status}_{conclusion}"), None);
    }
    let guard = match mutation_guard(
        runtime,
        observation,
        entry.pr_number,
        &entry.head_sha,
        &format!("queue drain rerun run {}", entry.run_id),
    ) {
        Ok(guard) => guard,
        Err(error) => return ("refused_by_mutation_guard".to_owned(), Some(error)),
    };
    match enqueued_now(actions, observation, entry.pr_number) {
        Ok(false) => {}
        Ok(true) => {
            // Not an error: the entry stays for a later resume.
            let _ = guard.finish("held_pull_request_enqueued");
            return ("held_pull_request_enqueued".to_owned(), None);
        }
        Err(error) => {
            let _ = guard.finish("live_recheck_failed");
            return ("live_recheck_failed".to_owned(), Some(error));
        }
    }
    // Re-running only cancelled/failed jobs keeps jobs that already passed.
    match actions.rerun_failed_jobs(&observation.repo, entry.run_id) {
        Ok(()) => ("rerun".to_owned(), guard.finish("rerun_accepted").err()),
        Err(error) => {
            let _ = guard.finish("rerun_rejected");
            ("rerun_failed".to_owned(), Some(error.to_string()))
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
