//! Reading the landing model off GitHub.
//!
//! Every read here is a `GET`. Nothing in this module enqueues, merges,
//! cancels, dispatches or writes: the command it serves is an instrument, and
//! an instrument that can change what it measures is a different and much more
//! dangerous tool.
//!
//! ## Budget
//!
//! | call | why |
//! |---|---|
//! | `rulesets` list | the only surface that names a ruleset-configured queue |
//! | `rulesets/{id}` per branch ruleset | the list omits `rules` entirely, so the list alone can never find a queue |
//! | `branches/{base}/protection` | strict up-to-date and the required contexts |
//! | one GraphQL query | default branch, effective merge queue, and the open-pull-request backlog |
//! | `actions/runs` | candidate runs to derive placement from |
//! | `runs/{id}/jobs`, bounded | the runner identity that actually picked each gate up |
//!
//! The job reads stop as soon as every required context has been placed, so a
//! healthy repository costs far fewer than the cap.

use serde_json::Value;

use crate::cloud::GitHubActions;
use crate::fleet_service::Boundary;
use crate::landing::placement::{self, JobObservation};
use crate::landing::queue::{self, Payload, QueueInputs};
use crate::landing::{LandingReport, SCHEMA_VERSION, SurfaceRead, backlog};

/// How many completed runs to consider when deriving placement.
pub const DEFAULT_RUN_SAMPLE: usize = 30;

/// How many per-run job reads to spend at most.
pub const DEFAULT_MAX_JOB_READS: usize = 20;

/// Everything the report needs, read once.
pub struct GatherOptions<'a> {
    /// `OWNER/REPO`.
    pub repo: &'a str,
    /// Base branch to model.
    pub base: &'a str,
    /// How many completed runs to consider.
    pub run_sample: usize,
    /// Upper bound on per-run job reads.
    pub max_job_reads: usize,
}

/// Read every surface and assemble the report.
#[must_use]
pub fn gather(actions: &GitHubActions, options: &GatherOptions<'_>) -> LandingReport {
    let mut surfaces: Vec<SurfaceRead> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut api_calls = 0u32;

    let repo = options.repo;
    let base = options.base;

    let mut inputs = QueueInputs::default();
    api_calls += read_rulesets(actions, repo, &mut inputs);

    api_calls += 1;
    let protection = read_protection(actions, repo, base);
    inputs.protection = protection.clone();

    // One GraphQL round trip for the default branch, the effective queue, and
    // the backlog. Three facts, one call, one rate-limit point.
    api_calls += 1;
    let graphql = read_graphql_facts(actions, repo, base, &mut inputs);
    let default_branch = graphql.default_branch;
    let backlog_finding = graphql.backlog;

    let merge_queue =
        queue::determine_queue(&inputs, base, default_branch.as_deref(), &mut surfaces);
    let strict = queue::determine_strict(&protection, &merge_queue);
    let enqueue = queue::enqueue_guidance(&merge_queue);

    let contexts = match &protection {
        Payload::Json(value) => placement::required_contexts(value),
        _ => Vec::new(),
    };
    let contexts_source = match &protection {
        Payload::Json(_) if contexts.is_empty() => "branch_protection (empty)".to_owned(),
        Payload::Json(_) => "branch_protection".to_owned(),
        Payload::NotFound => "none (branch is unprotected)".to_owned(),
        Payload::Unreadable(..) | Payload::NotConsulted => "unreadable".to_owned(),
    };

    let mut placement_notes = Vec::new();
    let (priority_runs, priority_calls) = runs_producing_contexts(
        actions,
        repo,
        &contexts,
        &graphql.open_pr_heads,
        &mut placement_notes,
    );
    api_calls += priority_calls;
    let (observations, read_error, spent) = observe_jobs(
        actions,
        repo,
        &contexts,
        &priority_runs,
        options,
        &mut placement_notes,
    );
    api_calls += spent;

    let checks = placement::classify_checks(
        &contexts,
        &observations,
        options.run_sample,
        read_error.as_ref(),
    );

    if graphql.base_exists == Some(false) {
        warnings.push(format!(
            "branch `{base}` does not exist on this repository{}; every finding below is the \
             well-formed nothing an absent branch returns, not a description of how work lands",
            default_branch
                .as_deref()
                .map_or_else(String::new, |branch| format!(
                    " (the default is `{branch}`)"
                ))
        ));
    }
    if matches!(protection, Payload::NotFound) {
        warnings.push(format!(
            "branch `{base}` carries no branch protection, so nothing on GitHub's side requires a \
             check before a merge"
        ));
    }

    LandingReport {
        schema_version: SCHEMA_VERSION,
        repo: repo.to_owned(),
        base: base.to_owned(),
        merge_queue,
        strict,
        enqueue,
        required_checks: placement::PlacementFinding {
            contexts_source,
            checks,
            notes: placement_notes,
        },
        backlog: backlog_finding,
        surfaces,
        warnings,
        api_calls,
    }
}

/// Warn when the base branch does not exist.
///
/// Only a confirmed absence warns. An unreadable existence check is silent
/// here, because warning on it would make every permission failure look like
/// a typo — and the report already says which surfaces it could not read.
#[must_use]
pub fn base_warning(
    base_exists: Option<bool>,
    base: &str,
    default_branch: Option<&str>,
) -> Option<String> {
    if base_exists != Some(false) {
        return None;
    }
    Some(format!(
        "branch `{base}` does not exist on this repository{}; every finding below is the \
         well-formed nothing an absent branch returns, not a description of how work lands",
        default_branch.map_or_else(String::new, |branch| format!(
            " (the default is `{branch}`)"
        ))
    ))
}

/// List the rulesets, then read each branch ruleset's detail.
///
/// The list call carries no `rules` array, so a merge queue is invisible until
/// the detail call. A detail call that fails leaves the whole surface unread:
/// the rule that matters may be in exactly the ruleset that could not be
/// fetched, and half a ruleset read is an unread one.
fn read_rulesets(actions: &GitHubActions, repo: &str, inputs: &mut QueueInputs) -> u32 {
    let mut calls = 1u32;
    let list = match read_json(actions, &format!("repos/{repo}/rulesets")) {
        Ok(list) => list,
        Err(error) => {
            inputs.rulesets_error = Some((classify(&error), error));
            return calls;
        }
    };
    let ids: Vec<u64> = list
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("target").and_then(Value::as_str) != Some("tag"))
                .filter_map(|item| item.get("id").and_then(Value::as_u64))
                .collect()
        })
        .unwrap_or_default();
    let mut details = Vec::new();
    let mut detail_error: Option<(Boundary, String)> = None;
    for id in ids {
        calls += 1;
        match read_json(actions, &format!("repos/{repo}/rulesets/{id}")) {
            Ok(detail) => details.push(detail),
            Err(error) => detail_error = Some((classify(&error), format!("ruleset {id}: {error}"))),
        }
    }
    if let Some(error) = detail_error {
        inputs.rulesets_error = Some(error);
    } else {
        inputs.rulesets = Some(details);
    }
    calls
}

/// Branch protection: strict, and the required contexts.
///
/// A `branch not protected` answer is a finding, not a failed read, and is
/// kept distinct from one.
fn read_protection(actions: &GitHubActions, repo: &str, base: &str) -> Payload {
    match read_json(actions, &format!("repos/{repo}/branches/{base}/protection")) {
        Ok(value) => Payload::Json(value),
        Err(error) => {
            if error.to_ascii_lowercase().contains("branch not protected") {
                Payload::NotFound
            } else {
                Payload::Unreadable(classify(&error), error)
            }
        }
    }
}

/// Default branch, effective merge queue, and the backlog, in one query.
fn read_graphql_facts(
    actions: &GitHubActions,
    repo: &str,
    base: &str,
    inputs: &mut QueueInputs,
) -> GraphqlFacts {
    match read_graphql(actions, repo, base) {
        Ok(value) => {
            inputs.graphql_queue = value
                .pointer("/data/repository/mergeQueue")
                .map_or(Payload::NotConsulted, |queue| Payload::Json(queue.clone()));
            let default_branch = value
                .pointer("/data/repository/defaultBranchRef/name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let backlog_finding = value.pointer("/data/repository/pullRequests").map_or_else(
                || {
                    backlog::unreadable(
                        Boundary::Parse,
                        "the GraphQL response carried no pullRequests connection".to_owned(),
                    )
                },
                backlog::from_graphql,
            );
            let head_shas = value
                .pointer("/data/repository/pullRequests/nodes")
                .and_then(Value::as_array)
                .map(|nodes| {
                    nodes
                        .iter()
                        .filter_map(|node| node.get("headRefOid")?.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let base_exists = value
                .pointer("/data/repository")
                .filter(|repository| !repository.is_null())
                .map(|repository| {
                    repository
                        .get("baseRef")
                        .is_some_and(|base_ref| !base_ref.is_null())
                });
            GraphqlFacts {
                base_exists,
                default_branch,
                backlog: backlog_finding,
                open_pr_heads: head_shas,
            }
        }
        Err(error) => {
            let boundary = classify(&error);
            inputs.graphql_queue = Payload::Unreadable(boundary, error.clone());
            GraphqlFacts {
                base_exists: None,
                default_branch: None,
                backlog: backlog::unreadable(boundary, error),
                open_pr_heads: Vec::new(),
            }
        }
    }
}

/// What the single GraphQL round trip yielded.
struct GraphqlFacts {
    /// Whether the base branch exists at all.
    ///
    /// Load-bearing: a base that does not exist answers every downstream
    /// question with a well-formed nothing — no protection, no queue, no open
    /// pull requests — and that reads as a repository with no gates rather
    /// than as a typo.
    base_exists: Option<bool>,
    /// The repository's default branch, needed to resolve `~DEFAULT_BRANCH`.
    default_branch: Option<String>,
    /// The open-pull-request backlog.
    backlog: backlog::BacklogFinding,
    /// Head commits of open pull requests, which is where a `pull_request`
    /// required context actually posts. The base branch's own head carries
    /// `push`-event checks instead, so it is the wrong place to look for one.
    open_pr_heads: Vec<String>,
}

/// Find the workflow runs that actually produced the required contexts.
///
/// A required `pull_request` context posts on a **pull request's head
/// commit**, not on the base branch's head — the base head carries the
/// `push`-event checks instead. So the cheapest precise route to "which run
/// produced `macos`" is one check-runs read per open pull request head, which
/// names the run directly and removes the guesswork from the sweep below.
fn runs_producing_contexts(
    actions: &GitHubActions,
    repo: &str,
    contexts: &[String],
    open_pr_heads: &[String],
    notes: &mut Vec<String>,
) -> (Vec<u64>, u32) {
    const MAX_HEADS: usize = 6;
    // More than one candidate per context, because a context is routinely
    // *satisfied* by a job that was skipped — a reuse path, or a conditional
    // that did not fire on that particular pull request. One candidate per
    // context would report such a gate as unplaceable even though it runs on
    // every other pull request in the backlog.
    const MAX_PER_CONTEXT: usize = 3;

    if contexts.is_empty() || open_pr_heads.is_empty() {
        return (Vec::new(), 0);
    }
    let mut calls = 0u32;
    let mut candidates: Vec<Vec<u64>> = vec![Vec::new(); contexts.len()];
    for sha in open_pr_heads.iter().take(MAX_HEADS) {
        if candidates.iter().all(|runs| runs.len() >= MAX_PER_CONTEXT) {
            break;
        }
        calls += 1;
        let payload = match read_json(
            actions,
            &format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"),
        ) {
            Ok(value) => value,
            Err(error) => {
                notes.push(format!("check runs for {sha} unreadable: {error}"));
                continue;
            }
        };
        let Some(check_runs) = payload.get("check_runs").and_then(Value::as_array) else {
            continue;
        };
        for check in check_runs {
            let Some(name) = check.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(index) = contexts.iter().position(|context| context == name) else {
                continue;
            };
            if candidates[index].len() >= MAX_PER_CONTEXT {
                continue;
            }
            if let Some(run_id) = run_id_from_url(check.get("html_url").and_then(Value::as_str))
                && !candidates[index].contains(&run_id)
            {
                candidates[index].push(run_id);
            }
        }
    }

    // Interleave, so every context gets a first attempt before any gets a
    // second. A budget spent three-deep on one gate while another has not been
    // looked at once is the wrong trade.
    let mut runs: Vec<u64> = Vec::new();
    for round in 0..MAX_PER_CONTEXT {
        for per_context in &candidates {
            if let Some(run_id) = per_context.get(round)
                && !runs.contains(run_id)
            {
                runs.push(*run_id);
            }
        }
    }
    (runs, calls)
}

/// Pull the run id out of a check run's `html_url`.
///
/// The trailing `/job/<id>` segment is deliberately ignored: the id there is
/// not always an Actions job id, and asking the jobs endpoint for something
/// that is not a job id returns a coherent, wrong answer rather than an error.
/// The run id in the path is unambiguous, and the run's own jobs listing is
/// the surface that carries runner identity.
fn run_id_from_url(url: Option<&str>) -> Option<u64> {
    let url = url?;
    let rest = url.split("/actions/runs/").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Read jobs until every required context has been placed or the budget runs
/// out, preferring runs already known to produce a required context.
fn observe_jobs(
    actions: &GitHubActions,
    repo: &str,
    contexts: &[String],
    priority_runs: &[u64],
    options: &GatherOptions<'_>,
    notes: &mut Vec<String>,
) -> (Vec<JobObservation>, Option<(Boundary, String)>, u32) {
    if contexts.is_empty() {
        return (Vec::new(), None, 0);
    }
    let mut calls = 0u32;
    let mut observations: Vec<JobObservation> = Vec::new();
    let mut reads = 0usize;
    let mut sweep_error: Option<(Boundary, String)> = None;

    let mut run_ids: Vec<u64> = priority_runs.to_vec();
    let mut swept = false;

    loop {
        for run_id in std::mem::take(&mut run_ids) {
            if reads >= options.max_job_reads || all_placed(contexts, &observations) {
                break;
            }
            reads += 1;
            calls += 1;
            match read_json(
                actions,
                &format!("repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"),
            ) {
                Ok(value) => observations.extend(placement::parse_jobs(&value.to_string(), run_id)),
                Err(error) => notes.push(format!("jobs for run {run_id} unreadable: {error}")),
            }
        }
        if swept || reads >= options.max_job_reads || all_placed(contexts, &observations) {
            break;
        }
        swept = true;
        calls += 1;
        let sample = options.run_sample;
        match read_json(
            actions,
            &format!("repos/{repo}/actions/runs?status=completed&per_page={sample}"),
        ) {
            Ok(value) => {
                run_ids = value
                    .get("workflow_runs")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|run| run.get("id").and_then(Value::as_u64))
                            .filter(|id| !priority_runs.contains(id))
                            .collect()
                    })
                    .unwrap_or_default();
            }
            Err(error) => {
                sweep_error = Some((classify(&error), error));
                break;
            }
        }
    }

    // A sweep that failed only matters when it was the last chance: contexts
    // already placed from a priority run are measured facts, and reporting
    // them as unknown because a later call failed would throw away a good
    // measurement.
    let read_error = sweep_error.filter(|_| observations.is_empty());
    if reads >= options.max_job_reads && !all_placed(contexts, &observations) {
        notes.push(format!(
            "stopped after {reads} per-run job reads; a context still reported as `no_evidence` \
             may simply not have run in that window"
        ));
    }
    (observations, read_error, calls)
}

fn all_placed(contexts: &[String], observations: &[JobObservation]) -> bool {
    contexts.iter().all(|context| {
        observations
            .iter()
            .any(|job| job.name == *context && job.runner_name.is_some())
    })
}

fn read_json(actions: &GitHubActions, path: &str) -> Result<Value, String> {
    let raw = actions
        .run_gh(&["api".to_owned(), path.to_owned()])
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed JSON from `{path}`: {error}"))
}

fn read_graphql(actions: &GitHubActions, repo: &str, base: &str) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    let query = format!(
        "query {{ repository(owner: \"{owner}\", name: \"{name}\") {{ \
           defaultBranchRef {{ name }} \
           baseRef: ref(qualifiedName: \"refs/heads/{base}\") {{ name }} \
           mergeQueue(branch: \"{base}\") {{ configuration {{ mergeMethod mergingStrategy \
             maximumEntriesToMerge maximumEntriesToBuild minimumEntriesToMerge \
             checkResponseTimeout }} }} \
           pullRequests(states: OPEN, first: 100, baseRefName: \"{base}\", \
             orderBy: {{ field: UPDATED_AT, direction: DESC }}) {{ totalCount \
             pageInfo {{ hasNextPage }} \
             nodes {{ number isDraft headRefOid mergeStateStatus \
             autoMergeRequest {{ enabledAt }} }} }} \
         }} }}"
    );
    let raw = actions
        .run_gh(&[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={query}"),
        ])
        .map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_str(&raw).map_err(|error| format!("malformed GraphQL JSON: {error}"))?;
    if let Some(errors) = value.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        return Err(format!(
            "GraphQL errors: {}",
            errors
                .iter()
                .filter_map(|error| error.get("message").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    Ok(value)
}

/// Map a GitHub API error to the boundary it actually hit.
///
/// A rate limit is transport, not permission, however much its `403` looks
/// like the latter: retrying later works, and telling an operator their token
/// lacks a scope sends them to fix something that is not broken.
#[must_use]
pub fn classify(error: &str) -> Boundary {
    let lower = error.to_ascii_lowercase();
    if lower.contains("rate limit") || lower.contains("secondary rate") {
        return Boundary::Transport;
    }
    if lower.contains("bad credentials")
        || lower.contains("401")
        || lower.contains("requires authentication")
    {
        return Boundary::Identity;
    }
    if lower.contains("resource not accessible")
        || lower.contains("must have admin rights")
        || lower.contains("403")
        || lower.contains("forbidden")
    {
        return Boundary::Permission;
    }
    if lower.contains("not supported") || lower.contains("unsupported") {
        return Boundary::Grammar;
    }
    if lower.contains("malformed json") || lower.contains("expected value") {
        return Boundary::Parse;
    }
    if lower.contains("404") || lower.contains("not found") {
        return Boundary::Scope;
    }
    Boundary::Transport
}
