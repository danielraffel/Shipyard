//! `shipyard diagnose`: a bounded diagnosis of a red pull request or run.
//!
//! Reads, through the app-authenticated reader every other command uses:
//!
//! - PR mode: one GraphQL read of the head's check contexts with
//!   `isRequired`; then, per workflow run that owns a failing required check,
//!   its jobs; per failing required job, its log; per cancelled job (and per
//!   runner-less sibling of a failed one), its check-run annotations, which
//!   carry GitHub's own reason for the cancel.
//! - Run mode: the run's jobs, the base branch's required-check policy (or
//!   `--required`), then the same per-job reads.
//!
//! Prints `shipyard.diagnose/v1` (see [`crate::diagnose`]), never larger than
//! `--max-bytes`. `--annotate plan` prints the check-run body instead;
//! `--annotate post` also creates the always-`neutral` "shipyard diagnose"
//! check run on the head, putting each failing test's evidence on its file and
//! line. Annotation is off by default.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Write;
use std::path::Path;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{DateTime, Duration, Utc};
use regex::Regex;
use serde_json::Value;

use super::CliFailure;
use crate::cloud::GitHubActions;
use crate::config::LoadedConfig;
use crate::diagnose::annotate::{self, Tree};
use crate::diagnose::{self, Context, Diagnosis, Job, Unreadable};
use crate::identity::RuntimeMode;
use crate::landability::gate;
use crate::pr_watch::flags::{DigestRoute, FlagKind};
use crate::pr_watch::ledger;
use crate::required_check_policy::{
    classic_required_checks, encode_path_segment, evaluated_required_checks,
    normalize_required_checks,
};

/// Config key: extra stale-base marker regexes.
pub(super) const STALE_MARKERS_KEY: &str = "diagnose.stale_base_markers";
/// Config key: extra fail-closed marker regexes.
pub(super) const FAIL_CLOSED_KEY: &str = "diagnose.fail_closed_markers";
/// How far back pr-watch's shared-failure flags count as corroboration.
const HISTORY_HOURS: i64 = 24;
/// Runner-less siblings whose annotations are read for one failed job.
const MAX_SIBLING_READS: usize = 3;

/// What to do with annotations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AnnotateMode {
    /// Nothing (default).
    Off,
    /// Print the check-run body instead of the diagnosis.
    Plan,
    /// Create the check run, then print the diagnosis.
    Post,
}

impl AnnotateMode {
    pub(super) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "off" => Ok(Self::Off),
            "plan" => Ok(Self::Plan),
            "post" => Ok(Self::Post),
            other => Err(format!(
                "--annotate must be off, plan or post, not `{other}`"
            )),
        }
    }
}

/// Parsed arguments.
pub(super) struct DiagnoseArgs {
    pub(super) pr: Option<u64>,
    pub(super) run: Option<u64>,
    pub(super) repo: Option<String>,
    pub(super) base: String,
    pub(super) required: Vec<String>,
    pub(super) max_bytes: usize,
    pub(super) annotate: AnnotateMode,
    pub(super) json: bool,
}

/// Run `shipyard diagnose`.
pub(super) fn diagnose_command<W: Write>(
    args: DiagnoseArgs,
    mode: RuntimeMode,
    cwd: &Path,
    state_dir: &Path,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    if args.pr.is_some() == args.run.is_some() {
        return Err(CliFailure::new(2, "pass exactly one of <PR> or --run <ID>"));
    }
    let config = LoadedConfig::load_from_cwd(mode, cwd)
        .map_err(|error| CliFailure::new(2, format!("config error: {error}")))?;
    let repo = args
        .repo
        .filter(|value| !value.trim().is_empty())
        .or_else(|| gate::resolve_repo(&config, cwd))
        .ok_or_else(|| CliFailure::new(1, "No repo detected. Pass --repo OWNER/REPO."))?;
    let mut context = Context::new();
    context.stale_markers = config_patterns(&config, STALE_MARKERS_KEY)?;
    context
        .fail_closed
        .extend(config_patterns(&config, FAIL_CLOSED_KEY)?);

    let actions = GitHubActions::from_loaded_config(cwd, &config);
    let calls = AtomicU32::new(0);
    let read = |gh_args: &[String]| {
        calls.fetch_add(1, Ordering::Relaxed);
        actions.run_gh(gh_args).map_err(|error| error.to_string())
    };

    let target = if let Some(pr) = args.pr {
        gather_pr(&read, &repo, pr)?
    } else {
        let run = args.run.unwrap_or_default();
        gather_run(&read, &repo, run, &args.base, &args.required)?
    };
    for (id, messages) in read_annotations(&read, &repo, &target) {
        context.annotations.insert(id, messages);
    }
    let this_pr = args.pr.or_else(|| target.jobs.iter().find_map(queue_pr));
    context.history = load_history(state_dir, &repo, &args.base, this_pr, Utc::now());
    let logs = read_logs(&read, &repo, &target);

    let mut doc = diagnose::build(
        &target.jobs,
        &logs,
        &target.required,
        &context,
        args.max_bytes,
    );
    doc.unreadable_checks
        .extend(target.unreadable.iter().cloned());
    doc.api_calls = Some(calls.load(Ordering::Relaxed));
    diagnose::fit(&mut doc, args.max_bytes);

    match args.annotate {
        AnnotateMode::Off => {}
        AnnotateMode::Plan => {
            let tree = head_tree(&read, &repo, &target.head_sha, cwd);
            let payload = annotate::check_run_payload(&doc, &target.head_sha, &tree);
            return print_json(stdout, &serde_json::to_value(&payload).unwrap_or_default());
        }
        AnnotateMode::Post => {
            let tree = head_tree(&read, &repo, &target.head_sha, cwd);
            let payload = annotate::check_run_payload(&doc, &target.head_sha, &tree);
            post_check_run(&actions, &repo, &payload)?;
        }
    }
    let written = if args.json {
        writeln!(stdout, "{}", doc.to_compact())
    } else {
        render_human(stdout, &doc)
    };
    written.map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn print_json<W: Write>(
    stdout: &mut W,
    value: &Value,
) -> Result<std::process::ExitCode, CliFailure> {
    writeln!(
        stdout,
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_default()
    )
    .map_err(|error| CliFailure::new(1, error.to_string()))?;
    Ok(std::process::ExitCode::SUCCESS)
}

fn config_patterns(config: &LoadedConfig, key: &str) -> Result<Vec<Regex>, CliFailure> {
    let Some(value) = config.get(key) else {
        return Ok(Vec::new());
    };
    let items = value
        .as_array()
        .ok_or_else(|| CliFailure::new(2, format!("{key} must be an array of regex strings")))?;
    items
        .iter()
        .map(|item| {
            let text = item
                .as_str()
                .ok_or_else(|| CliFailure::new(2, format!("{key} must hold strings")))?;
            Regex::new(text).map_err(|error| {
                CliFailure::new(2, format!("{key}: bad pattern `{text}`: {error}"))
            })
        })
        .collect()
}

type Read<'a> = dyn Fn(&[String]) -> Result<String, String> + 'a;

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn read_json(read: &Read<'_>, parts: &[&str]) -> Result<Value, String> {
    let text = read(&argv(parts))?;
    serde_json::from_str(&text).map_err(|error| format!("parse {}: {error}", parts.join(" ")))
}

/// Everything the pure diagnosis needs, gathered.
#[derive(Debug, Default)]
pub(super) struct Target {
    pub(super) head_sha: String,
    pub(super) jobs: Vec<Job>,
    pub(super) required: Vec<String>,
    pub(super) unreadable: Vec<Unreadable>,
}

/// GraphQL: the head and every check context with its `isRequired`.
const PR_QUERY: &str = "query($owner:String!,$name:String!,$number:Int!){repository(owner:$owner,name:$name){pullRequest(number:$number){headRefOid commits(last:1){nodes{commit{oid statusCheckRollup{contexts(first:100){pageInfo{hasNextPage} nodes{__typename ... on CheckRun{databaseId name status conclusion detailsUrl isRequired(pullRequestNumber:$number)} ... on StatusContext{context state isRequired(pullRequestNumber:$number)}}}}}}}}}}";

static JOB_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/actions/runs/(\d+)/job/(\d+)").expect("job url pattern"));
static QUEUE_BRANCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/pr-(\d+)-[0-9a-f]{40}$").expect("queue branch pattern"));

/// A required context that is not green, from the PR rollup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RedContext {
    pub(super) name: String,
    pub(super) run_id: Option<u64>,
    pub(super) state: String,
}

/// What the PR query says about the head.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Rollup {
    pub(super) head: String,
    pub(super) required: Vec<String>,
    pub(super) failing: Vec<RedContext>,
    pub(super) unreadable: Vec<Unreadable>,
}

/// The head, the required context names, the required contexts that are not
/// green, and what could not be read, from the PR query.
pub(super) fn parse_pr_rollup(value: &Value) -> Result<Rollup, String> {
    let pull = value
        .pointer("/data/repository/pullRequest")
        .ok_or("pull request not found")?;
    let head = pull
        .get("headRefOid")
        .and_then(Value::as_str)
        .ok_or("pull request has no head")?
        .to_owned();
    let contexts = pull.pointer("/commits/nodes/0/commit/statusCheckRollup/contexts");
    let mut unreadable = Vec::new();
    if contexts
        .and_then(|c| c.pointer("/pageInfo/hasNextPage"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        unreadable.push(Unreadable {
            context: "*".to_owned(),
            reason: "head has more than 100 check contexts; later ones were not read".to_owned(),
        });
    }
    let mut required = BTreeSet::new();
    let mut red = Vec::new();
    for node in contexts
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if node.get("isRequired").and_then(Value::as_bool) != Some(true) {
            continue;
        }
        let check_run = node.get("__typename").and_then(Value::as_str) == Some("CheckRun");
        let name = node
            .get(if check_run { "name" } else { "context" })
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        required.insert(name.clone());
        let state = if check_run {
            node.get("conclusion").and_then(Value::as_str)
        } else {
            node.get("state").and_then(Value::as_str)
        }
        .unwrap_or_default()
        .to_ascii_lowercase();
        let bad = matches!(
            state.as_str(),
            "failure" | "cancelled" | "timed_out" | "error" | "action_required" | "startup_failure"
        );
        if !bad {
            continue;
        }
        let run_id = node
            .get("detailsUrl")
            .and_then(Value::as_str)
            .and_then(|url| JOB_URL.captures(url))
            .and_then(|caps| caps[1].parse().ok());
        if !check_run || run_id.is_none() {
            unreadable.push(Unreadable {
                context: name.clone(),
                reason: format!("{state}; not a GitHub Actions job, no log to read"),
            });
            continue;
        }
        red.push(RedContext {
            name,
            run_id,
            state,
        });
    }
    Ok(Rollup {
        head,
        required: required.into_iter().collect(),
        failing: red,
        unreadable,
    })
}

fn gather_pr(read: &Read<'_>, repo: &str, pr: u64) -> Result<Target, CliFailure> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| CliFailure::new(2, format!("repo `{repo}` is not OWNER/REPO")))?;
    let rollup = read_json(
        read,
        &[
            "api",
            "graphql",
            "-f",
            &format!("query={PR_QUERY}"),
            "-F",
            &format!("owner={owner}"),
            "-F",
            &format!("name={name}"),
            "-F",
            &format!("number={pr}"),
        ],
    )
    .map_err(|error| CliFailure::new(1, error))?;
    let Rollup {
        head,
        required,
        failing,
        unreadable,
    } = parse_pr_rollup(&rollup).map_err(|error| CliFailure::new(1, error))?;
    let mut target = Target {
        head_sha: head,
        required,
        unreadable,
        ..Target::default()
    };
    let runs: BTreeSet<u64> = failing
        .iter()
        .filter_map(|context| context.run_id)
        .collect();
    for run in runs {
        match read_jobs(read, repo, run) {
            Ok(jobs) => target.jobs.extend(jobs),
            Err(error) => target.unreadable.extend(
                failing
                    .iter()
                    .filter(|context| context.run_id == Some(run))
                    .map(|context| Unreadable {
                        context: context.name.clone(),
                        reason: format!("jobs of run {run} unreadable: {error}"),
                    }),
            ),
        }
    }
    Ok(target)
}

fn read_jobs(read: &Read<'_>, repo: &str, run: u64) -> Result<Vec<Job>, String> {
    let value = read_json(
        read,
        &[
            "api",
            &format!("repos/{repo}/actions/runs/{run}/jobs?per_page=100"),
        ],
    )?;
    let jobs = value
        .get("jobs")
        .cloned()
        .unwrap_or(Value::Array(Vec::new()));
    serde_json::from_value(jobs).map_err(|error| format!("jobs of run {run}: {error}"))
}

fn gather_run(
    read: &Read<'_>,
    repo: &str,
    run: u64,
    base: &str,
    required: &[String],
) -> Result<Target, CliFailure> {
    let jobs = read_jobs(read, repo, run).map_err(|error| CliFailure::new(1, error))?;
    let required = if required.is_empty() {
        required_policy(read, repo, base).map_err(|error| CliFailure::new(1, error))?
    } else {
        required.to_vec()
    };
    Ok(Target {
        head_sha: jobs
            .iter()
            .find_map(|job| job.head_sha.clone())
            .unwrap_or_default(),
        jobs,
        required,
        unreadable: Vec::new(),
    })
}

fn required_policy(read: &Read<'_>, repo: &str, base: &str) -> Result<Vec<String>, String> {
    let base = encode_path_segment(base);
    let evaluated = read_json(
        read,
        &[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{repo}/rules/branches/{base}"),
        ],
    )?;
    let mut checks = evaluated_required_checks(&evaluated)?;
    match read_json(
        read,
        &[
            "api",
            &format!("repos/{repo}/branches/{base}/protection/required_status_checks"),
        ],
    ) {
        Ok(classic) => checks.extend(classic_required_checks(&classic)?),
        Err(error) if error.contains("404") => {}
        Err(error) => return Err(error),
    }
    Ok(normalize_required_checks(checks)
        .into_iter()
        .map(|check| check.context)
        .collect())
}

/// The jobs whose check-run annotations decide a classification: every
/// cancelled required job, and up to [`MAX_SIBLING_READS`] runner-less
/// cancelled siblings of each failed required job.
pub(super) fn annotation_targets(target: &Target) -> Vec<i64> {
    let required = |job: &Job| target.required.contains(&job.name);
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for job in target.jobs.iter().filter(|job| required(job)) {
        match job.conclusion.as_deref() {
            Some("cancelled") => {
                ids.insert(job.id);
            }
            Some("failure") => ids.extend(
                target
                    .jobs
                    .iter()
                    .filter(|sibling| sibling.run_id == job.run_id && sibling.runnerless())
                    .take(MAX_SIBLING_READS)
                    .map(|sibling| sibling.id),
            ),
            _ => {}
        }
    }
    ids.into_iter().collect()
}

fn read_annotations(read: &Read<'_>, repo: &str, target: &Target) -> Vec<(i64, Vec<String>)> {
    annotation_targets(target)
        .into_iter()
        .filter_map(|id| {
            let value = read_json(
                read,
                &[
                    "api",
                    &format!("repos/{repo}/check-runs/{id}/annotations?per_page=50"),
                ],
            )
            .ok()?;
            let messages = value
                .as_array()?
                .iter()
                .filter_map(|item| item.get("message").and_then(Value::as_str))
                .map(str::to_owned)
                .collect();
            Some((id, messages))
        })
        .collect()
}

/// Each failing required job's log, keyed by job id. A job that never ran has
/// none.
fn read_logs(read: &Read<'_>, repo: &str, target: &Target) -> HashMap<i64, String> {
    target
        .jobs
        .iter()
        .filter(|job| {
            target.required.contains(&job.name) && job.is_bad() && !job.steps().is_empty()
        })
        .take(diagnose::MAX_CHECKS)
        .filter_map(|job| {
            read(&argv(&[
                "api",
                &format!("repos/{repo}/actions/jobs/{}/logs", job.id),
            ]))
            .ok()
            .map(|log| (job.id, log))
        })
        .collect()
}

fn queue_pr(job: &Job) -> Option<u64> {
    let branch = job.head_branch.as_deref()?;
    QUEUE_BRANCH.captures(branch)?[1].parse().ok()
}

/// Tests that failed on OTHER pull requests in the last day, from pr-watch's
/// shared-failure flags. A missing or unreadable ledger gives no history,
/// which only ever withholds `flake_candidate`.
pub(super) fn history_from_ledger(
    ledger: &ledger::Ledger,
    this_pr: Option<u64>,
    now: DateTime<Utc>,
) -> HashMap<String, Vec<u64>> {
    let since = now - Duration::hours(HISTORY_HOURS);
    let mut out: BTreeMap<String, BTreeSet<u64>> = BTreeMap::new();
    for entry in ledger.entries.values() {
        if entry.kind != FlagKind::RepeatTestFailure
            || entry.route != DigestRoute::Shared
            || entry.last_seen_at < since
        {
            continue;
        }
        let prs: BTreeSet<u64> = entry
            .related_prs
            .iter()
            .copied()
            .chain([entry.pr])
            .filter(|pr| Some(*pr) != this_pr)
            .collect();
        if prs.is_empty() {
            continue;
        }
        for test in &entry.shared_tests {
            out.entry(test.clone())
                .or_default()
                .extend(prs.iter().copied());
        }
    }
    out.into_iter()
        .map(|(test, prs)| (test, prs.into_iter().collect()))
        .collect()
}

fn load_history(
    state_dir: &Path,
    repo: &str,
    base: &str,
    this_pr: Option<u64>,
    now: DateTime<Utc>,
) -> HashMap<String, Vec<u64>> {
    let path = ledger::default_path(state_dir, repo, base);
    ledger::load(&path, repo, base)
        .map(|ledger| history_from_ledger(&ledger, this_pr, now))
        .unwrap_or_default()
}

/// The head's paths: from the local clone when it has the commit, else one
/// recursive tree read. A truncated tree gives an empty one, because a
/// suffix that is unique in part of a tree may not be unique in all of it.
fn head_tree(read: &Read<'_>, repo: &str, sha: &str, cwd: &Path) -> Tree {
    if sha.is_empty() {
        return Tree::default();
    }
    let local = std::process::Command::new("git")
        .args(["ls-tree", "-r", "--name-only", sha])
        .current_dir(cwd)
        .output();
    if let Ok(output) = local
        && output.status.success()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        return Tree::new(text.lines().filter(|line| !line.is_empty()));
    }
    let Ok(value) = read_json(
        read,
        &["api", &format!("repos/{repo}/git/trees/{sha}?recursive=1")],
    ) else {
        return Tree::default();
    };
    if value.get("truncated").and_then(Value::as_bool) != Some(false) {
        return Tree::default();
    }
    Tree::new(
        value
            .get("tree")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("blob"))
            .filter_map(|item| item.get("path").and_then(Value::as_str))
            .map(str::to_owned),
    )
}

fn post_check_run(
    actions: &GitHubActions,
    repo: &str,
    payload: &annotate::CheckRunPayload,
) -> Result<(), CliFailure> {
    let body = serde_json::to_string(payload)
        .map_err(|error| CliFailure::new(1, format!("check-run body: {error}")))?;
    let mut file = tempfile::NamedTempFile::new()
        .map_err(|error| CliFailure::new(1, format!("check-run body file: {error}")))?;
    file.write_all(body.as_bytes())
        .map_err(|error| CliFailure::new(1, format!("check-run body file: {error}")))?;
    let path = file.path().to_string_lossy().into_owned();
    actions
        .run_gh(&argv(&[
            "api",
            "--method",
            "POST",
            &format!("repos/{repo}/check-runs"),
            "--input",
            &path,
        ]))
        .map_err(|error| CliFailure::new(1, format!("post check run: {error}")))?;
    Ok(())
}

fn render_human<W: Write>(stdout: &mut W, doc: &Diagnosis) -> std::io::Result<()> {
    writeln!(
        stdout,
        "diagnose: {}: {}",
        doc.verdict.to_uppercase(),
        doc.summary
    )?;
    for check in &doc.checks {
        let tests = &check.failing_tests;
        writeln!(
            stdout,
            "- {} [{}/{}]{}",
            check.context,
            check.classification.class,
            check.classification.rule,
            check
                .failing_step
                .as_ref()
                .map(|step| format!(" step '{}'", step.name))
                .unwrap_or_default()
        )?;
        writeln!(stdout, "  why: {}", check.classification.why)?;
        if tests.total > 0 {
            writeln!(
                stdout,
                "  tests ({}): {}",
                tests.total,
                tests.names.join(", ")
            )?;
        }
        if let Some(group) = check.evidence.first() {
            writeln!(stdout, "  evidence (line {}):", group.line)?;
            for line in &group.lines {
                writeln!(stdout, "    {line}")?;
            }
        }
        writeln!(stdout, "  more: {}", check.more.fetch)?;
    }
    for name in &doc.omitted_checks {
        writeln!(stdout, "- {name} [omitted to stay within the byte cap]")?;
    }
    for item in &doc.unreadable_checks {
        writeln!(stdout, "- {} [unreadable: {}]", item.context, item.reason)?;
    }
    if doc.advisory_failures.count > 0 {
        writeln!(
            stdout,
            "advisory failures ({}): {}",
            doc.advisory_failures.count,
            doc.advisory_failures.names.join(", ")
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
