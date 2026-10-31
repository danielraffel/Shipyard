//! `shipyard train`: the merge train's switch, its read-only status and
//! metrics, and the host pause. See [`crate::merge_train`].
//!
//! Nothing here mutates GitHub. `status --record` appends one sample to a
//! local file; `pause` and `resume` write and remove the guards-dir pause
//! file. While the train is off for a repository, `status` makes at most the
//! one base-config read, and none at all under a kill switch.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use serde::Serialize;
use serde_json::Value;

use super::observation::observe_repo;
use crate::app::CliFailure;
use crate::auto_arm::{ArmSkip, ArmVerdict, decide_from_queue_state};
use crate::cloud::GitHubActions;
use crate::merge_steward::{StewardPolicy, classify_pr};
use crate::merge_train::{
    KILL_ENV, Merge, OffBy, PAUSE_FILE, PrSample, Sample, Stage, TrainSwitch, enabled_in_config,
    kill_switch, metrics, read_pause, stage_from_decision,
};
use crate::pr_queue_state::{PR_QUEUE_STATE_QUERY, explain_pr_queue_state};

/// A `gh` transport: stdout, or a message that includes stderr.
type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

/// What `shipyard train` was asked to do.
pub(crate) enum TrainAction {
    /// Print the switch and, when on, every open pull request's stage.
    Status {
        /// `OWNER/REPO`.
        repo: String,
        /// Base branch.
        base: String,
        /// Append this pass to the repository's sample file.
        record: bool,
    },
    /// Print metrics over the recorded samples.
    Metrics {
        /// `OWNER/REPO`.
        repo: String,
        /// Window, in hours, ending now.
        since_hours: u32,
    },
    /// Write the guards-dir pause file.
    Pause {
        /// Why.
        reason: String,
    },
    /// Remove the guards-dir pause file: only by whoever paused, and only
    /// with a reason.
    Resume {
        /// Why.
        reason: String,
    },
}

/// This host's kill switch inputs: [`KILL_ENV`] and the pause file's path.
pub(crate) struct Host {
    /// [`KILL_ENV`]'s value.
    pub(crate) kill_env: Option<String>,
    /// Where the pause file lives, when there is a guards directory.
    pub(crate) pause: Option<PathBuf>,
    /// Who is running this command (`user@host`), recorded on a pause and
    /// required to match it on a resume.
    pub(crate) operator: String,
}

impl Host {
    /// The real host: the environment and the guards directory.
    pub(crate) fn from_env() -> Self {
        Self {
            kill_env: std::env::var(KILL_ENV).ok(),
            pause: crate::head_approval::guards_dir().map(|dir| dir.join(PAUSE_FILE)),
            operator: operator(),
        }
    }
}

/// One open pull request in a status report.
#[derive(Clone, Debug, Serialize)]
struct StageRow {
    number: u64,
    head: String,
    stage: Stage,
    reason: String,
    /// For a pull request handed to the steward: whether its handoff names
    /// the current head, carries to it, or is missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    handoff: Option<String>,
}

/// Run one `shipyard train` action.
pub(crate) fn train_command<W: Write>(
    actions: &GitHubActions,
    action: TrainAction,
    host: &Host,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    match action {
        TrainAction::Status { repo, base, record } => {
            status(actions, host, &repo, &base, record, state_dir, json, stdout)
        }
        TrainAction::Metrics { repo, since_hours } => {
            metrics_report(&repo, since_hours, state_dir, json, stdout)
        }
        TrainAction::Pause { reason } => pause(host, &reason, json, stdout),
        TrainAction::Resume { reason } => resume(host, &reason, json, stdout),
    }
}

/// The train's switch for `repo`: the kill switch first, with no GitHub
/// read, then the base config, read through the API.
fn switch(run_gh: RunGh<'_>, host: &Host, repo: &str, base: &str) -> TrainSwitch {
    let pause = host.pause.as_deref().and_then(read_pause);
    if let Some(by) = kill_switch(host.kill_env.as_deref(), pause, repo) {
        return TrainSwitch::Off { by };
    }
    match run_gh(&[
        "api".to_owned(),
        "-H".to_owned(),
        "Accept: application/vnd.github.raw+json".to_owned(),
        format!("repos/{repo}/contents/.shipyard/config.toml?ref={base}"),
    ]) {
        Ok(text) => match enabled_in_config(&text) {
            Ok(true) => TrainSwitch::On,
            Ok(false) => TrainSwitch::Off {
                by: OffBy::ConfigOff,
            },
            Err(detail) => TrainSwitch::Off {
                by: OffBy::ConfigUnreadable { detail },
            },
        },
        Err(detail) if detail.contains("404") || detail.contains("Not Found") => TrainSwitch::Off {
            by: OffBy::ConfigOff,
        },
        Err(detail) => TrainSwitch::Off {
            by: OffBy::ConfigUnreadable { detail },
        },
    }
}

#[allow(clippy::too_many_arguments)]
fn status<W: Write>(
    actions: &GitHubActions,
    host: &Host,
    repo: &str,
    base: &str,
    record: bool,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let run_gh = |args: &[String]| actions.run_gh(args).map_err(|error| error.to_string());
    let train = switch(&run_gh, host, repo, base);
    if !train.is_on() {
        return emit(
            stdout,
            json,
            &serde_json::json!({"repo": repo, "base": base, "train": train}),
            &format!("train {repo} ({base}): {}", train.explain()),
        );
    }
    let observation = observe_repo(actions, repo, base, false)
        .map_err(|error| CliFailure::new(1, format!("could not observe {repo}: {error}")))?;
    if !observation.merge_queue {
        return emit(
            stdout,
            json,
            &serde_json::json!({"repo": repo, "base": base, "train": train,
                "applicable": false, "detail": "the base has no merge queue"}),
            &format!("train {repo} ({base}): on, but not applicable: the base has no merge queue"),
        );
    }
    let rows = stage_rows(&run_gh, &observation);
    let queue_depth = rows.iter().filter(|row| row.stage == Stage::Queued).count() as u64;
    if record {
        record_sample(&run_gh, repo, base, queue_depth, &rows, state_dir)?;
    }
    let mut counts: BTreeMap<Stage, usize> = BTreeMap::new();
    for row in &rows {
        *counts.entry(row.stage).or_insert(0) += 1;
    }
    let mut text = format!(
        "train {repo} ({base}): {} | queue depth {queue_depth} | {}",
        train.explain(),
        counts
            .iter()
            .map(|(stage, count)| format!("{}={count}", stage_name(*stage)))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for row in &rows {
        let _ = write!(
            text,
            "\n  #{} {}: {}",
            row.number,
            stage_name(row.stage),
            row.reason
        );
    }
    emit(
        stdout,
        json,
        &serde_json::json!({"repo": repo, "base": base, "train": train, "applicable": true,
            "queue_depth": queue_depth, "counts": counts, "prs": rows, "recorded": record}),
        &text,
    )
}

/// Every open pull request's stage. The steward's own classification decides
/// until it would arm; then the queue state, `head_approval`'s evaluation
/// (merge-only and rebase carry included, never verdict-text matching) and
/// the tier rule decide whether it is ready.
fn stage_rows(run_gh: RunGh<'_>, observation: &super::RepoObservation) -> Vec<StageRow> {
    let policy = StewardPolicy {
        merge_queue: observation.merge_queue,
        native_auto_merge: observation.allow_auto_merge,
        auto_merge_approval_context: crate::merge_steward::DEFAULT_AUTO_MERGE_APPROVAL_CONTEXT
            .to_owned(),
        required_checks: observation.required_checks.clone(),
        opt_out_label: "shipyard:no-auto-merge".to_owned(),
        provenance_blocking_labels: vec![super::DEFAULT_PROVENANCE_BLOCKING_LABEL.to_owned()],
        managed_label: None,
        handoff_context: String::new(),
        max_transient_reruns: 1,
    };
    let approval = crate::head_approval::read_policy(run_gh, &observation.repo, &observation.base);
    let mut rows: Vec<StageRow> = observation
        .prs
        .iter()
        .map(|pr| {
            let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
            let (stage, reason) = match stage_from_decision(&decision) {
                Some(stage) => (stage, format!("{decision:?}")),
                None => armable_stage(run_gh, observation, pr, approval.as_ref()),
            };
            StageRow {
                number: pr.fact.number,
                head: pr.fact.head_sha.clone(),
                stage,
                reason,
                handoff: handoff_state(run_gh, &observation.repo, pr),
            }
        })
        .collect();
    rows.sort_by_key(|row| (row.stage, row.number));
    rows
}

/// For a pull request carrying the management label, whether its steward
/// handoff names its head. `None` for an unmanaged pull request.
fn handoff_state(run_gh: RunGh<'_>, repo: &str, pr: &super::ObservedPr) -> Option<String> {
    let managed = pr
        .fact
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(super::MANAGED_LABEL));
    if !managed {
        return None;
    }
    if crate::merge_steward::has_successful_status(&pr.fact, super::HANDOFF_CONTEXT) {
        return Some("on the head".to_owned());
    }
    let carry = |from: &str, to: &str| {
        crate::head_approval::merge_only_carry(run_gh, repo, pr.fact.number, from, to)
    };
    Some(
        match carried_handoff(run_gh, repo, pr.fact.number, &pr.fact.head_sha, &carry) {
            Ok(Some(from)) => format!(
                "carries from {}: every commit since is a clean merge of the base",
                &from[..from.len().min(12)]
            ),
            Ok(None) => "missing: no earlier head of this pull request was handed off".to_owned(),
            Err(why) => format!("missing on the head and does not carry ({why})"),
        },
    )
}

/// Commits of one pull request searched for an earlier handed-off head.
const HANDOFF_SEARCH_COMMITS: usize = 20;

/// The handoff status is a commit status, so it names one head: a clean
/// merge of the base makes a new head with none, and the steward reads the
/// pull request as `HandoffMissing` although its approval carries. This
/// finds the newest earlier commit of the pull request with a successful
/// handoff status and returns it when `carry(from, head)` accepts the
/// commits since: the same merge-only rule `head_approval` applies to an
/// approval (every commit a merge of the base, every changed file at its
/// handed-off blob and mode).
///
/// # Errors
///
/// When the commits or statuses cannot be read, or the newest handed-off
/// head does not carry (its reason).
fn carried_handoff(
    run_gh: RunGh<'_>,
    repo: &str,
    pr: u64,
    head: &str,
    carry: &dyn Fn(&str, &str) -> Result<(), String>,
) -> Result<Option<String>, String> {
    let raw = run_gh(&[
        "api".to_owned(),
        format!("repos/{repo}/pulls/{pr}/commits?per_page=100"),
    ])?;
    let commits: Value =
        serde_json::from_str(&raw).map_err(|error| format!("commits were not JSON: {error}"))?;
    let shas: Vec<&str> = commits
        .as_array()
        .ok_or_else(|| "the commits endpoint did not answer a list".to_owned())?
        .iter()
        .filter_map(|commit| commit.get("sha").and_then(Value::as_str))
        .filter(|sha| *sha != head)
        .collect();
    for sha in shas.iter().rev().take(HANDOFF_SEARCH_COMMITS) {
        let raw = run_gh(&[
            "api".to_owned(),
            format!("repos/{repo}/commits/{sha}/status"),
        ])?;
        let status: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("status of {sha} was not JSON: {error}"))?;
        let handed_off = status
            .get("statuses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|entry| {
                entry.get("context").and_then(Value::as_str) == Some(super::HANDOFF_CONTEXT)
                    && entry.get("state").and_then(Value::as_str) == Some("success")
            });
        if handed_off {
            return carry(sha, head).map(|()| Some((*sha).to_owned()));
        }
    }
    Ok(None)
}

/// The stage of a pull request the steward would arm.
fn armable_stage(
    run_gh: RunGh<'_>,
    observation: &super::RepoObservation,
    pr: &super::ObservedPr,
    approval: Result<&crate::head_approval::ApprovalPolicy, &String>,
) -> (Stage, String) {
    let repo = &observation.repo;
    let number = pr.fact.number;
    let head = &pr.fact.head_sha;
    let queue = match queue_state(run_gh, repo, number) {
        Ok(queue) => queue,
        Err(detail) => return (Stage::Held, format!("queue state unreadable: {detail}")),
    };
    match decide_from_queue_state(&explain_pr_queue_state(&queue).state, pr.fact.draft) {
        ArmVerdict::Arm => {}
        ArmVerdict::Skip(skip) => {
            let stage = match skip {
                ArmSkip::AlreadyQueued { .. } => Stage::Queued,
                ArmSkip::AlreadyArmed { .. } => Stage::Armed,
                ArmSkip::EjectedSameHead { .. } => Stage::EjectedSameHead,
                ArmSkip::Draft => Stage::Draft,
                _ => Stage::Held,
            };
            return (stage, skip.explain());
        }
    }
    let approval = match approval {
        Ok(policy) => policy,
        Err(detail) => return (Stage::Held, format!("approval policy unreadable: {detail}")),
    };
    // A report never writes: the carry is judged, never logged as a carry.
    let mut approval = approval.clone();
    approval.dry_run = true;
    let approved = if approval.required {
        match crate::head_approval::evaluate_with_policy_for_author(
            run_gh,
            repo,
            number,
            head,
            &approval,
            &queue,
            pr.fact.author.as_deref(),
        ) {
            Ok(gate) if gate.allows() => gate.explain(),
            Ok(gate) => return (Stage::NeedsReview, gate.explain()),
            Err(detail) => return (Stage::Held, format!("approval unreadable: {detail}")),
        }
    } else {
        "this repository does not require head approval".to_owned()
    };
    match crate::deferred_tier::deferred_tier_checks(run_gh, repo, number, &observation.base, head)
    {
        Ok(checks) => crate::deferred_tier::explain_all(&checks)
            .map_or((Stage::Ready, approved), |detail| {
                (Stage::TierBlocked, detail)
            }),
        Err(detail) => (Stage::Held, format!("test tier unreadable: {detail}")),
    }
}

/// One pull request's [`PR_QUEUE_STATE_QUERY`] response.
fn queue_state(run_gh: RunGh<'_>, repo: &str, number: u64) -> Result<Value, String> {
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| format!("repo `{repo}` is not OWNER/REPO"))?;
    let raw = run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={PR_QUEUE_STATE_QUERY}"),
        "-F".to_owned(),
        format!("owner={owner}"),
        "-F".to_owned(),
        format!("name={name}"),
        "-F".to_owned(),
        format!("number={number}"),
    ])?;
    serde_json::from_str(&raw).map_err(|error| format!("malformed GraphQL JSON: {error}"))
}

/// Append this pass, plus every merge since the previous sample, to the
/// repository's sample file.
fn record_sample(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    queue_depth: u64,
    rows: &[StageRow],
    state_dir: &Path,
) -> Result<(), CliFailure> {
    let path = samples_path(state_dir, repo);
    let previous = read_samples(&path).last().map_or_else(
        || (Utc::now() - Duration::hours(1)).to_rfc3339(),
        |sample| sample.at.clone(),
    );
    let merged = merges_since(run_gh, repo, base, &previous)
        .map_err(|detail| CliFailure::new(1, format!("could not read merges: {detail}")))?;
    let sample = Sample {
        at: Utc::now().to_rfc3339(),
        queue_depth,
        prs: rows
            .iter()
            .map(|row| {
                (
                    row.number,
                    PrSample {
                        stage: row.stage,
                        head: row.head.clone(),
                    },
                )
            })
            .collect(),
        merged,
    };
    append_sample(&path, &sample)
        .map_err(|error| CliFailure::new(1, format!("could not record a sample: {error}")))
}

/// Pull requests merged into `base` at or after `since`.
fn merges_since(
    run_gh: RunGh<'_>,
    repo: &str,
    base: &str,
    since: &str,
) -> Result<Vec<Merge>, String> {
    let day = since.get(..19).unwrap_or(since);
    let query = format!(
        "query{{search(query:\"repo:{repo} is:pr is:merged base:{base} merged:>={day}\",type:ISSUE,first:100){{nodes{{... on PullRequest{{number mergedAt}}}}}}}}"
    );
    let raw = run_gh(&[
        "api".to_owned(),
        "graphql".to_owned(),
        "-f".to_owned(),
        format!("query={query}"),
    ])?;
    let value: Value = serde_json::from_str(&raw).map_err(|error| error.to_string())?;
    let nodes = value
        .pointer("/data/search/nodes")
        .and_then(Value::as_array)
        .ok_or_else(|| "the merged-pull-request search returned no nodes".to_owned())?;
    Ok(nodes
        .iter()
        .filter_map(|node| {
            Some(Merge {
                number: node.get("number")?.as_u64()?,
                merged_at: node.get("mergedAt")?.as_str()?.to_owned(),
            })
        })
        .collect())
}

fn metrics_report<W: Write>(
    repo: &str,
    since_hours: u32,
    state_dir: &Path,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let cutoff = Utc::now() - Duration::hours(i64::from(since_hours));
    let samples: Vec<Sample> = read_samples(&samples_path(state_dir, repo))
        .into_iter()
        .filter(|sample| {
            chrono::DateTime::parse_from_rfc3339(&sample.at)
                .is_ok_and(|at| at.with_timezone(&Utc) >= cutoff)
        })
        .collect();
    let measured = metrics(&samples);
    let text = format!(
        "train {repo}, last {since_hours}h: {} samples over {:.1}h | {} merges ({:.2}/h) | \
         approved->merged p50 {} / p95 {} min (n={}) | queue empty while ready {} min | \
         batches {:?} | heads per merged PR {}",
        measured.samples,
        measured.hours,
        measured.merges,
        measured.merges_per_hour,
        measured
            .approved_to_merged_p50_minutes
            .map_or_else(|| "-".to_owned(), |minutes| minutes.to_string()),
        measured
            .approved_to_merged_p95_minutes
            .map_or_else(|| "-".to_owned(), |minutes| minutes.to_string()),
        measured.latency_population,
        measured.queue_empty_while_ready_minutes,
        measured.batch_sizes,
        measured
            .heads_per_merged_pr
            .map_or_else(|| "-".to_owned(), |heads| format!("{heads:.2}"))
    );
    emit(
        stdout,
        json,
        &serde_json::json!({"repo": repo, "since_hours": since_hours, "metrics": measured}),
        &text,
    )
}

fn pause<W: Write>(
    host: &Host,
    reason: &str,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(CliFailure::new(
            2,
            "`shipyard train pause` needs a --reason",
        ));
    }
    let path = host
        .pause
        .clone()
        .ok_or_else(|| CliFailure::new(1, "no guards directory to write the pause file in"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| CliFailure::new(1, format!("could not create guards dir: {error}")))?;
    }
    let mut record = crate::head_approval::writer_identity();
    record.insert("reason".to_owned(), Value::from(reason));
    record.insert("at".to_owned(), Value::from(Utc::now().to_rfc3339()));
    record.insert("by".to_owned(), Value::from(host.operator.clone()));
    let record = Value::Object(record);
    std::fs::write(&path, record.to_string())
        .map_err(|error| CliFailure::new(1, format!("could not write the pause file: {error}")))?;
    audit(&path, "pause", &record);
    emit(
        stdout,
        json,
        &serde_json::json!({"paused": true, "path": path, "reason": reason}),
        &format!("train paused on this host ({})", path.display()),
    )
}

/// Lift this host's pause. Refused while [`KILL_ENV`] still kills the train
/// (a resume would change nothing), without a reason, and by anyone but the
/// operator who paused; an unparseable pause file is removed by hand.
fn resume<W: Write>(
    host: &Host,
    reason: &str,
    json: bool,
    stdout: &mut W,
) -> Result<std::process::ExitCode, CliFailure> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(CliFailure::new(
            2,
            "`shipyard train resume` needs a --reason",
        ));
    }
    if let Some(value) = host.kill_env.as_deref().filter(|value| kills_any(value)) {
        return Err(CliFailure::new(
            1,
            format!("{KILL_ENV}={value} still kills the train here; unset it, not the pause file"),
        ));
    }
    let path = host
        .pause
        .clone()
        .ok_or_else(|| CliFailure::new(1, "no guards directory holds a pause file"))?;
    if path.exists() {
        let paused_by = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|record| record.get("by")?.as_str().map(str::to_owned));
        match paused_by {
            Some(by) if by == host.operator => {}
            Some(by) => {
                return Err(CliFailure::new(
                    1,
                    format!(
                        "the train was paused by {by}; only they resume it (this is {})",
                        host.operator
                    ),
                ));
            }
            None => {
                return Err(CliFailure::new(
                    1,
                    format!(
                        "{} names no pauser; inspect it and remove it by hand",
                        path.display()
                    ),
                ));
            }
        }
    }
    let removed = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(CliFailure::new(
                1,
                format!("could not remove the pause file: {error}"),
            ));
        }
    };
    if removed {
        let mut record = crate::head_approval::writer_identity();
        record.insert("reason".to_owned(), Value::from(reason));
        record.insert("at".to_owned(), Value::from(Utc::now().to_rfc3339()));
        record.insert("by".to_owned(), Value::from(host.operator.clone()));
        audit(&path, "resume", &Value::Object(record));
    }
    emit(
        stdout,
        json,
        &serde_json::json!({"paused": false, "removed": removed, "path": path}),
        &if removed {
            format!("train resumed on this host (removed {})", path.display())
        } else {
            "train was not paused on this host".to_owned()
        },
    )
}

/// Whether a [`KILL_ENV`] value kills at least one repository.
fn kills_any(value: &str) -> bool {
    kill_switch(Some(value), None, "").is_some()
        || value.trim().to_ascii_lowercase().starts_with("off:")
}

/// Append one pause or resume to `train-pauses.jsonl` beside the pause file.
/// Never fails the command.
fn audit(pause: &Path, action: &str, record: &Value) {
    use std::io::Write as _;
    let Some(dir) = pause.parent() else {
        return;
    };
    let mut entry = record.clone();
    if let Some(map) = entry.as_object_mut() {
        map.insert("action".to_owned(), Value::from(action));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("train-pauses.jsonl"))
    {
        let _ = writeln!(file, "{entry}");
    }
}

fn operator() -> String {
    let user = std::env::var("USER").unwrap_or_else(|_| "an unknown user".to_owned());
    let host = std::process::Command::new("hostname")
        .arg("-s")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| "an unknown host".to_owned());
    format!("{user}@{host}")
}

fn samples_path(state_dir: &Path, repo: &str) -> PathBuf {
    state_dir
        .join("train")
        .join(format!("{}.jsonl", repo.replace('/', "__")))
}

fn read_samples(path: &Path) -> Vec<Sample> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn append_sample(path: &Path, sample: &Sample) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let line = serde_json::to_string(sample).map_err(std::io::Error::other)?;
    writeln!(file, "{line}")
}

fn stage_name(stage: Stage) -> String {
    serde_json::to_value(stage)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn emit<W: Write>(
    stdout: &mut W,
    json: bool,
    value: &Value,
    text: &str,
) -> Result<std::process::ExitCode, CliFailure> {
    let rendered = if json {
        serde_json::to_string_pretty(value).unwrap_or_default()
    } else {
        text.to_owned()
    };
    writeln!(stdout, "{rendered}")
        .map_err(|error| CliFailure::new(1, format!("could not write output: {error}")))?;
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests;
