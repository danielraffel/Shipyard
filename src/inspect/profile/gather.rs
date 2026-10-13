//! The bounded reads behind the profile.
//!
//! Five settings and file reads, then at most two per sampled workflow:
//!
//! | read | endpoint |
//! |---|---|
//! | commit the base resolves to | `repos/{repo}/commits/{base}` |
//! | file list at that commit | `repos/{repo}/git/trees/{sha}?recursive=1` |
//! | every workflow's text, one GraphQL call | `object(expression: "{sha}:.github/workflows")` |
//! | active rules | `repos/{repo}/rules/branches/{base}` |
//! | classic required checks | `repos/{repo}/branches/{base}/protection/required_status_checks` |
//! | latest completed run of a workflow | `repos/{repo}/actions/workflows/{file}/runs` |
//! | that run's jobs | `repos/{repo}/actions/runs/{id}/jobs` |
//!
//! Runs are read only for workflows with a computed `runs-on:` (a routing
//! variable or a job output), since a literal says where the job runs. The
//! workflows producing a required check are read first; the rest follow by
//! name until the budget is spent, and the ones left are named.

use std::cell::Cell;

use serde_json::Value;

use super::{RunSample, SampledRun, Snapshot, WorkflowFile, runs_on_is_computed};
use crate::landability::workflow::parse_workflow_jobs;
use crate::validation_signals::GhReader;

/// Default number of workflows whose latest run is read.
pub const DEFAULT_MAX_RUNS: usize = 8;

const WORKFLOWS_QUERY: &str = "query($owner:String!,$name:String!,$expr:String!){repository(\
     owner:$owner,name:$name){object(expression:$expr){... on Tree{entries{name type object{\
     ... on Blob{byteSize isBinary isTruncated text}}}}}}}";

/// What to read.
pub struct GatherOptions<'a> {
    /// `OWNER/REPO`.
    pub repo: &'a str,
    /// Branch.
    pub base: &'a str,
    /// Workflows whose latest run may be read.
    pub max_runs: usize,
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn json(gh: &GhReader<'_>, args: &[String]) -> Result<Value, String> {
    let text = gh(args)?;
    serde_json::from_str(&text).map_err(|error| format!("unparseable JSON from GitHub: {error}"))
}

fn api(gh: &GhReader<'_>, path: &str) -> Result<Value, String> {
    json(gh, &strings(&["api", path]))
}

/// Whether a read error means "this branch has no protection".
fn is_unprotected(error: &str) -> bool {
    error.contains("Branch not protected") || error.contains("HTTP 404") || error.contains("(404)")
}

/// Read everything the profile needs; returns the snapshot and the calls spent.
#[must_use]
pub fn gather(gh: &GhReader<'_>, options: &GatherOptions<'_>) -> (Snapshot, u32) {
    let calls = Cell::new(0u32);
    let counted = |args: &[String]| {
        calls.set(calls.get() + 1);
        gh(args)
    };
    let gh: &GhReader<'_> = &counted;
    let repo = options.repo;
    let base = options.base;

    let commit = api(gh, &format!("repos/{repo}/commits/{base}")).and_then(|value| {
        value
            .get("sha")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| "the commit read carried no sha".to_owned())
    });
    // Fall back to the branch name so the rest is still read, at a commit
    // the report then says it could not pin.
    let reference = commit.clone().unwrap_or_else(|_| base.to_owned());

    let paths = read_paths(gh, repo, &reference);
    let workflows = read_workflows(gh, repo, &reference, &paths);
    let rules = api(gh, &format!("repos/{repo}/rules/branches/{base}"));
    let protection = match api(
        gh,
        &format!("repos/{repo}/branches/{base}/protection/required_status_checks"),
    ) {
        Ok(value) => Ok(Some(value)),
        Err(error) if is_unprotected(&error) => Ok(None),
        Err(error) => Err(error),
    };
    let required = required_contexts(&rules, &protection);
    let runs = read_runs(gh, repo, &workflows, &required, options.max_runs);
    (
        Snapshot {
            commit,
            paths,
            workflows,
            rules,
            protection,
            runs,
        },
        calls.get(),
    )
}

fn read_paths(
    gh: &GhReader<'_>,
    repo: &str,
    reference: &str,
) -> Result<(Vec<String>, bool), String> {
    let tree = api(
        gh,
        &format!("repos/{repo}/git/trees/{reference}?recursive=1"),
    )?;
    let entries = tree
        .get("tree")
        .and_then(Value::as_array)
        .ok_or_else(|| "the file list read carried no tree".to_owned())?;
    let paths = entries
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("blob"))
        .filter_map(|entry| entry.get("path").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let truncated = tree
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok((paths, truncated))
}

fn is_workflow_file(path: &str) -> bool {
    path.strip_prefix(".github/workflows/").is_some_and(|name| {
        !name.contains('/')
            && std::path::Path::new(name).extension().is_some_and(|ext| {
                ext.eq_ignore_ascii_case("yml") || ext.eq_ignore_ascii_case("yaml")
            })
    })
}

fn read_workflows(
    gh: &GhReader<'_>,
    repo: &str,
    reference: &str,
    paths: &Result<(Vec<String>, bool), String>,
) -> Result<Vec<WorkflowFile>, String> {
    let listed = match paths {
        Ok((paths, _)) => paths.iter().filter(|path| is_workflow_file(path)).count(),
        Err(error) => return Err(format!("the file list was not read ({error})")),
    };
    if listed == 0 {
        return Ok(Vec::new());
    }
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("`{repo}` is not OWNER/REPO"))?;
    let answer = json(
        gh,
        &[
            "api".to_owned(),
            "graphql".to_owned(),
            "-f".to_owned(),
            format!("query={WORKFLOWS_QUERY}"),
            "-f".to_owned(),
            format!("owner={owner}"),
            "-f".to_owned(),
            format!("name={name}"),
            "-f".to_owned(),
            format!("expr={reference}:.github/workflows"),
        ],
    )?;
    if let Some(errors) = answer.get("errors").filter(|errors| !errors.is_null()) {
        return Err(format!("GraphQL answered with errors: {errors}"));
    }
    let entries = answer
        .pointer("/data/repository/object/entries")
        .and_then(Value::as_array)
        .ok_or_else(|| "the workflow read carried no entries".to_owned())?;
    let workflows: Vec<WorkflowFile> = entries
        .iter()
        .filter_map(|entry| {
            let file = entry.get("name").and_then(Value::as_str)?;
            let path = format!(".github/workflows/{file}");
            if !is_workflow_file(&path) {
                return None;
            }
            let blob = entry.get("object").cloned().unwrap_or(Value::Null);
            let binary = blob.get("isBinary").and_then(Value::as_bool) == Some(true);
            let truncated = blob.get("isTruncated").and_then(Value::as_bool) == Some(true);
            let text = blob.get("text").and_then(Value::as_str).map(str::to_owned);
            let note = if binary {
                Some("binary".to_owned())
            } else if truncated {
                Some("text truncated by GitHub".to_owned())
            } else if text.is_none() {
                Some("no text returned".to_owned())
            } else {
                None
            };
            Some(WorkflowFile {
                path,
                text: if note.is_none() { text } else { None },
                note,
            })
        })
        .collect();
    // Control: the file list and the text read must agree on what exists.
    if workflows.len() != listed {
        return Err(format!(
            "the file list holds {listed} workflow file(s) but the text read returned {}",
            workflows.len()
        ));
    }
    Ok(workflows)
}

fn required_contexts(
    rules: &Result<Value, String>,
    protection: &Result<Option<Value>, String>,
) -> Vec<String> {
    let mut contexts: Vec<String> = Vec::new();
    if let Some(checks) = rules
        .as_ref()
        .ok()
        .and_then(|rules| crate::required_check_policy::evaluated_required_checks(rules).ok())
    {
        contexts.extend(checks.into_iter().map(|check| check.context));
    }
    if let Some(checks) = protection
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .and_then(|protection| {
            crate::required_check_policy::classic_required_checks(protection).ok()
        })
    {
        contexts.extend(checks.into_iter().map(|check| check.context));
    }
    contexts.sort();
    contexts.dedup();
    contexts
}

fn read_runs(
    gh: &GhReader<'_>,
    repo: &str,
    workflows: &Result<Vec<WorkflowFile>, String>,
    required: &[String],
    max_runs: usize,
) -> RunSample {
    let Ok(workflows) = workflows else {
        return RunSample::default();
    };
    // (produces a required check, file name)
    let mut wanted: Vec<(bool, String)> = workflows
        .iter()
        .filter_map(|workflow| {
            let text = workflow.text.as_deref()?;
            let jobs = parse_workflow_jobs(text);
            let computed = jobs
                .iter()
                .any(|job| job.runs_on_expr.as_deref().is_some_and(runs_on_is_computed));
            if !computed {
                return None;
            }
            let required = jobs
                .iter()
                .any(|job| required.iter().any(|context| job.produces(context)));
            let file = workflow
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&workflow.path)
                .to_owned();
            Some((!required, file))
        })
        .collect();
    wanted.sort();
    let mut sample = RunSample {
        wanted: wanted.iter().map(|(_, file)| file.clone()).collect(),
        ..RunSample::default()
    };
    for (index, (_, file)) in wanted.iter().enumerate() {
        if index >= max_runs {
            sample.over_budget.push(file.clone());
            continue;
        }
        let listed = api(
            gh,
            &format!("repos/{repo}/actions/workflows/{file}/runs?status=completed&per_page=1"),
        );
        let run_id = match listed {
            Ok(value) => value
                .get("workflow_runs")
                .and_then(Value::as_array)
                .and_then(|runs| runs.first())
                .and_then(|run| run.get("id"))
                .and_then(Value::as_u64),
            Err(error) => {
                sample.errors.push((file.clone(), error));
                continue;
            }
        };
        let Some(run_id) = run_id else {
            sample.no_runs.push(file.clone());
            continue;
        };
        match api(
            gh,
            &format!("repos/{repo}/actions/runs/{run_id}/jobs?per_page=100"),
        ) {
            Ok(value) => {
                let jobs: Vec<Value> = value
                    .get("jobs")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let total = value.get("total_count").and_then(Value::as_u64);
                sample.sampled.push(SampledRun {
                    workflow: file.clone(),
                    run_id,
                    partial: total.is_some_and(|total| total > jobs.len() as u64),
                    jobs,
                });
            }
            Err(error) => sample.errors.push((file.clone(), error)),
        }
    }
    sample
}
