//! Decide whether one same-head re-enqueue is safe after an *environment*
//! ejection.
//!
//! ## The problem this answers
//!
//! Under `ALLGREEN` grouping a head the queue ejected for `failed_checks` is
//! refused a same-head re-enqueue: re-entering the queue with the head that
//! broke a batch breaks the next batch too, and takes its batch-mates with it
//! ([`crate::pr_queue_state::same_head_requeue_cascades`]). That is right when
//! the head, or a flaky test, broke the batch. It is wrong when the batch died
//! because the network did: a package relay answered 403, a download host
//! did not resolve, an upload connection was reset. The head is as good as it
//! was, and the only way back into the queue was to push a new head and pay a
//! whole required-gate cycle for a commit that changes nothing.
//!
//! ## What counts as an environment failure
//!
//! Positive evidence only, never absence. Every failing **required** check on
//! the ejecting merge-group commit must be a GitHub Actions job whose every
//! failing step printed an [`ENVIRONMENT_SIGNATURES`] line within
//! [`FAILURE_PROXIMITY_LINES`] output lines of that step's first `##[error]`.
//! One required failure without that evidence refuses the whole verdict, and
//! so does anything that could not be read.
//!
//! Three details carry the safety:
//!
//! * **Output, not script.** A `run:` step's log opens with a
//!   `##[group]Run …` block that echoes the step's own script, and a script can
//!   contain the signature: Pulp's `Install visual-analysis Python
//!   dependencies` step carries a comment quoting `Tunnel connection failed:
//!   403 Forbidden`. Lines inside that echo are never read as output.
//! * **The failing step, located by the API.** A `continue-on-error` or
//!   `if: always()` step can print its own `##[error]` after the real one, so
//!   "the first `##[error]` in the log" is not the failing step. The step is
//!   taken from the jobs API (`conclusion == failure`) and its output from the
//!   first `Run` segment that starts at or after the step's `started_at`,
//!   through the first `##[error]` in or after it.
//! * **Near the failure.** A long `Build` step can print a retried, recovered
//!   network warning and then fail to compile. Only the tail before the error
//!   is read, so a signature thousands of lines earlier explains nothing.
//!
//! ## Bounded
//!
//! A repository's own content can produce a signature (a head that points a
//! download at a host that does not exist). So the allowance is one
//! re-enqueue per head: [`crate::pr_queue_state::PrQueueReport::ejections_of_current_head`]
//! must be exactly one, over a timeline window that reached the start of
//! history. A head the queue ejected twice has had its retry, and the
//! ordinary refusal applies. The worst a wrong allowance can cost is the one
//! batch the retry joins.
//!
//! ## Opt-in
//!
//! Off unless the repository sets `queue.environment_requeue.enabled = true`
//! in its Shipyard config. Which failures the network can cause and a diff
//! cannot is a property of the repository's CI, and a repository whose CI
//! fetches from hosts its own content names should not inherit the allowance
//! silently.

use serde::Serialize;
use serde_json::Value;

use crate::pr_queue_state::{PrQueueReport, PrQueueState};

/// Shipyard config key that opts a repository in.
pub const CONFIG_KEY: &str = "queue.environment_requeue.enabled";

/// Transport failures that say the network, not the caller, failed.
///
/// Shared with [`crate::classify`], whose SSH/infra classifier matches the
/// same spellings on a validation leg's stderr. Case-sensitive: these are the
/// exact spellings libc, OpenSSH and curl print.
pub const NETWORK_TRANSPORT_MARKERS: [&str; 4] = [
    "Could not resolve host",
    "Network is unreachable",
    "No route to host",
    "Connection reset by peer",
];

/// Job-log lines that prove a step failed on the network.
///
/// [`NETWORK_TRANSPORT_MARKERS`] plus the spellings package managers and
/// download tools use: Node (`ENOTFOUND`, `EAI_AGAIN`, `ECONNRESET`,
/// `getaddrinfo`), pip through a proxy (`Tunnel connection failed`), curl
/// (`curl: (6)` could not resolve, `curl: (56)` receive failure,
/// `Proxy CONNECT aborted`), and glibc's resolver.
///
/// Deliberately **not** the fleet-read `TRANSIENT_MARKERS`
/// (`app::fleet_status_cmd::readability`): `timeout`, `timed out`,
/// `rate limit` and `server error` are words a test log prints when the head
/// is at fault, and `Connection refused` / `Connection timed out` are what a
/// test prints when a server the head broke never came up.
pub const ENVIRONMENT_SIGNATURES: [&str; 13] = [
    NETWORK_TRANSPORT_MARKERS[0],
    NETWORK_TRANSPORT_MARKERS[1],
    NETWORK_TRANSPORT_MARKERS[2],
    NETWORK_TRANSPORT_MARKERS[3],
    "ENOTFOUND",
    "getaddrinfo",
    "EAI_AGAIN",
    "Tunnel connection failed",
    "Proxy CONNECT aborted",
    "Temporary failure in name resolution",
    "ECONNRESET",
    "curl: (6)",
    "curl: (56)",
];

/// Output lines before (and including) a failing step's first `##[error]`
/// that may carry the signature.
pub const FAILURE_PROXIMITY_LINES: usize = 60;

const RUN_GROUP: &str = "##[group]Run ";
const END_GROUP: &str = "##[endgroup]";
const ERROR: &str = "##[error]";

/// One signature line found in a failing step's output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SignatureHit {
    /// The [`ENVIRONMENT_SIGNATURES`] entry that matched.
    pub signature: String,
    /// The output line it matched, timestamp stripped, trimmed to 240 chars.
    pub line: String,
}

/// What one failing step's log says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "reading", rename_all = "snake_case")]
pub enum StepReading {
    /// An environment signature near the step's failure.
    Environment(SignatureHit),
    /// The step's output was found and carries no signature near its failure.
    NoSignature,
    /// No `Run` segment with an `##[error]` starts at or after the step's
    /// `started_at` (a runner-generated step, or a log that is not this job's).
    NotLocated,
}

/// Split a job log line into its `YYYY-MM-DDTHH:MM:SS` prefix and the text.
fn split_timestamp(raw: &str) -> (Option<&str>, &str) {
    let raw = raw.trim_start_matches('\u{feff}');
    let bytes = raw.as_bytes();
    let looks_stamped = bytes.len() >= 20
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':';
    if !looks_stamped {
        return (None, raw);
    }
    let Some(end) = raw.find('Z') else {
        return (None, raw);
    };
    let text = raw[end + 1..].strip_prefix(' ').unwrap_or(&raw[end + 1..]);
    (Some(&raw[..19]), text)
}

struct Segment<'a> {
    started: Option<&'a str>,
    output: Vec<&'a str>,
    has_error: bool,
}

/// The log's `Run` segments: each `##[group]Run` header with the output that
/// follows its closing `##[endgroup]`, script echo excluded.
fn segments(log: &str) -> Vec<Segment<'_>> {
    let mut segments: Vec<Segment<'_>> = Vec::new();
    let mut in_echo = false;
    for raw in log.lines() {
        let (stamp, text) = split_timestamp(raw);
        if text.starts_with(RUN_GROUP) {
            segments.push(Segment {
                started: stamp,
                output: Vec::new(),
                has_error: false,
            });
            in_echo = true;
            continue;
        }
        let Some(current) = segments.last_mut() else {
            continue;
        };
        if in_echo {
            if text.starts_with(END_GROUP) {
                in_echo = false;
            }
            continue;
        }
        if text.starts_with(ERROR) {
            current.has_error = true;
        }
        current.output.push(text);
    }
    segments
}

/// The output of the step that started at `started_at` (the jobs API's
/// `steps[].started_at`), through its first `##[error]` line. `None` when no
/// segment at or after `started_at` carries an error.
#[must_use]
pub fn failing_step_output<'a>(log: &'a str, started_at: &str) -> Option<Vec<&'a str>> {
    let started_at = started_at.get(..19)?;
    let mut output = Vec::new();
    for segment in segments(log) {
        if segment.started.is_none_or(|stamp| stamp < started_at) {
            continue;
        }
        if segment.has_error {
            let error = segment
                .output
                .iter()
                .position(|line| line.starts_with(ERROR))
                .unwrap_or(segment.output.len().saturating_sub(1));
            output.extend_from_slice(&segment.output[..=error]);
            return Some(output);
        }
        output.extend(segment.output);
    }
    None
}

/// The first signature in the last [`FAILURE_PROXIMITY_LINES`] lines of
/// `output`, searching from the failure backwards.
#[must_use]
pub fn signature_near_failure(output: &[&str]) -> Option<SignatureHit> {
    let tail = &output[output.len().saturating_sub(FAILURE_PROXIMITY_LINES)..];
    tail.iter().rev().find_map(|line| {
        ENVIRONMENT_SIGNATURES
            .iter()
            .find(|signature| line.contains(**signature))
            .map(|signature| SignatureHit {
                signature: (*signature).to_owned(),
                line: line.trim().chars().take(240).collect(),
            })
    })
}

/// Read one failing step out of its job's log.
#[must_use]
pub fn read_failing_step(log: &str, started_at: &str) -> StepReading {
    match failing_step_output(log, started_at) {
        None => StepReading::NotLocated,
        Some(output) => signature_near_failure(&output)
            .map_or(StepReading::NoSignature, StepReading::Environment),
    }
}

/// One failing step of one failing required check, and what its log says.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StepEvidence {
    /// The required check (job) name.
    pub check: String,
    /// The Actions job id (equal to the check-run id).
    pub job_id: u64,
    /// The failing step's name.
    pub step: String,
    /// What its output says.
    #[serde(flatten)]
    pub reading: StepReading,
}

impl StepEvidence {
    /// One line for a human.
    #[must_use]
    pub fn render(&self) -> String {
        let what = match &self.reading {
            StepReading::Environment(hit) => {
                format!("environment ({}): {}", hit.signature, hit.line)
            }
            StepReading::NoSignature => format!(
                "no environment signature within {FAILURE_PROXIMITY_LINES} lines of its failure"
            ),
            StepReading::NotLocated => "its output could not be located in the job log".to_owned(),
        };
        format!(
            "{} / {} [job {}]: {what}",
            self.check, self.step, self.job_id
        )
    }
}

/// Whether one same-head re-enqueue is allowed after this ejection, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EnvironmentRequeue {
    /// Whether the re-enqueue is allowed.
    pub allowed: bool,
    /// Why, in one sentence.
    pub reason: String,
    /// The merge-group commit whose required checks were read.
    pub merge_group_commit: Option<String>,
    /// Per failing step of each failing required check, what its log says.
    pub evidence: Vec<StepEvidence>,
}

impl EnvironmentRequeue {
    fn refuse(reason: impl Into<String>, merge_group_commit: Option<String>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            merge_group_commit,
            evidence: Vec::new(),
        }
    }
}

/// A `gh` transport: stdout, or a message that includes stderr.
pub type RunGh<'a> = &'a dyn Fn(&[String]) -> Result<String, String>;

fn api(run_gh: RunGh<'_>, path: String) -> Result<String, String> {
    run_gh(&["api".to_owned(), path])
}

fn api_json(run_gh: RunGh<'_>, path: &str) -> Result<Value, String> {
    let raw = api(run_gh, path.to_owned())?;
    serde_json::from_str(&raw).map_err(|error| format!("{path} was not JSON: {error}"))
}

/// Whether the report is the case this module speaks to: an open pull request
/// the queue ejected for `failed_checks` with no new head since.
#[must_use]
pub fn applies(report: &PrQueueReport) -> bool {
    matches!(
        &report.state,
        PrQueueState::Ejected { reason, new_head_since_removal: false, .. }
            if reason.eq_ignore_ascii_case("failed_checks")
    )
}

/// The checks that gate `base`, from rulesets plus classic protection.
fn required_contexts(run_gh: RunGh<'_>, repo: &str, base: &str) -> Result<Vec<String>, String> {
    let mut contexts = Vec::new();
    let rules = api_json(run_gh, &format!("repos/{repo}/rules/branches/{base}"))?;
    for rule in rules.as_array().into_iter().flatten() {
        if rule.get("type").and_then(Value::as_str) != Some("required_status_checks") {
            continue;
        }
        for check in rule
            .pointer("/parameters/required_status_checks")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(context) = check.get("context").and_then(Value::as_str) {
                contexts.push(context.to_owned());
            }
        }
    }
    match api_json(
        run_gh,
        &format!("repos/{repo}/branches/{base}/protection/required_status_checks"),
    ) {
        Ok(classic) => {
            for context in classic
                .get("contexts")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                contexts.push(context.to_owned());
            }
        }
        Err(error) if error.contains("404") => {}
        Err(error) => return Err(error),
    }
    contexts.sort();
    contexts.dedup();
    Ok(contexts)
}

/// A required check that did not pass on the merge-group commit.
struct FailedCheck {
    name: String,
    id: Option<u64>,
    conclusion: String,
    actions_job: bool,
}

fn failed_required_checks(
    run_gh: RunGh<'_>,
    repo: &str,
    sha: &str,
    required: &[String],
) -> Result<Vec<FailedCheck>, String> {
    let runs = api_json(
        run_gh,
        &format!("repos/{repo}/commits/{sha}/check-runs?per_page=100"),
    )?;
    let list = runs
        .get("check_runs")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("check runs of {sha} carried no check_runs"))?;
    if runs.get("total_count").and_then(Value::as_u64) != Some(list.len() as u64) {
        return Err(format!(
            "{sha} has more check runs than one page returned; refusing a partial reading"
        ));
    }
    let mut failed = Vec::new();
    for context in required {
        // Re-runs leave several check runs under one name; the newest decides.
        let Some(latest) = list
            .iter()
            .filter(|run| run.get("name").and_then(Value::as_str) == Some(context.as_str()))
            .max_by_key(|run| run.get("id").and_then(Value::as_u64).unwrap_or(0))
        else {
            continue;
        };
        if latest.get("status").and_then(Value::as_str) != Some("completed") {
            return Err(format!(
                "required check `{context}` on {sha} has not completed"
            ));
        }
        let conclusion = latest
            .get("conclusion")
            .and_then(Value::as_str)
            .unwrap_or("none")
            .to_owned();
        if matches!(conclusion.as_str(), "success" | "neutral" | "skipped") {
            continue;
        }
        failed.push(FailedCheck {
            name: context.clone(),
            id: latest.get("id").and_then(Value::as_u64),
            conclusion,
            actions_job: latest.pointer("/app/slug").and_then(Value::as_str)
                == Some("github-actions"),
        });
    }
    let statuses = api_json(run_gh, &format!("repos/{repo}/commits/{sha}/status"))?;
    for status in statuses
        .get("statuses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let context = status.get("context").and_then(Value::as_str).unwrap_or("");
        let state = status.get("state").and_then(Value::as_str).unwrap_or("");
        if required.iter().any(|required| required == context)
            && matches!(state, "failure" | "error")
        {
            failed.push(FailedCheck {
                name: context.to_owned(),
                id: None,
                conclusion: state.to_owned(),
                actions_job: false,
            });
        }
    }
    Ok(failed)
}

/// Read every failing step of one failing required Actions job.
fn read_job(
    run_gh: RunGh<'_>,
    repo: &str,
    check: &str,
    job_id: u64,
) -> Result<Vec<StepEvidence>, String> {
    let job = api_json(run_gh, &format!("repos/{repo}/actions/jobs/{job_id}"))?;
    let failing = job
        .get("steps")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|step| step.get("conclusion").and_then(Value::as_str) == Some("failure"))
        .collect::<Vec<_>>();
    if failing.is_empty() {
        return Err(format!(
            "`{check}` [job {job_id}] failed with no failing step recorded, which is not \
             evidence of anything"
        ));
    }
    let log = api(run_gh, format!("repos/{repo}/actions/jobs/{job_id}/logs")).map_err(|error| {
        format!("the log of `{check}` [job {job_id}] could not be read ({error})")
    })?;
    Ok(failing
        .into_iter()
        .map(|step| StepEvidence {
            check: check.to_owned(),
            job_id,
            step: step
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            reading: step
                .get("started_at")
                .and_then(Value::as_str)
                .map_or(StepReading::NotLocated, |started| {
                    read_failing_step(&log, started)
                }),
        })
        .collect())
}

/// Decide the one environment re-enqueue for `report`, reading the ejecting
/// merge group's required checks and job logs through `run_gh`.
///
/// `None` when the report is not a same-head `failed_checks` ejection
/// ([`applies`]); the ordinary guidance stands untouched there. Every other
/// answer carries its reason, allowed or not.
#[must_use]
pub fn assess(
    run_gh: RunGh<'_>,
    repo: &str,
    report: &PrQueueReport,
    opted_in: bool,
) -> Option<EnvironmentRequeue> {
    if !applies(report) {
        return None;
    }
    let commit = report
        .last_ejection
        .as_ref()
        .and_then(|ejection| ejection.merge_group_commit.clone());
    if !opted_in {
        return Some(EnvironmentRequeue::refuse(
            format!(
                "this repository has not opted in to environment re-enqueues (`{CONFIG_KEY} = \
                 true` in its Shipyard config)"
            ),
            commit,
        ));
    }
    if report.timeline_complete != Some(true) {
        return Some(EnvironmentRequeue::refuse(
            "the timeline window does not reach the start of history, so an earlier ejection \
             of this head cannot be ruled out",
            commit,
        ));
    }
    if report.ejections_of_current_head != 1 {
        return Some(EnvironmentRequeue::refuse(
            format!(
                "the queue has ejected this head {} times; the one environment re-enqueue a \
                 head is allowed has been spent",
                report.ejections_of_current_head
            ),
            commit,
        ));
    }
    let Some(sha) = commit.clone() else {
        return Some(EnvironmentRequeue::refuse(
            "the removal names no merge-group commit, so the checks that ejected it cannot be \
             read",
            None,
        ));
    };
    Some(match assess_merge_group(run_gh, repo, report, &sha) {
        Ok(verdict) => verdict,
        Err(detail) => EnvironmentRequeue::refuse(detail, commit),
    })
}

fn assess_merge_group(
    run_gh: RunGh<'_>,
    repo: &str,
    report: &PrQueueReport,
    sha: &str,
) -> Result<EnvironmentRequeue, String> {
    let pr = report
        .pr
        .ok_or_else(|| "the pull request number is unknown".to_owned())?;
    let pull = api_json(run_gh, &format!("repos/{repo}/pulls/{pr}"))?;
    let base = pull
        .pointer("/base/ref")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("PR #{pr} carries no base ref"))?;
    let required = required_contexts(run_gh, repo, base)
        .map_err(|error| format!("the required checks of `{base}` could not be read ({error})"))?;
    if required.is_empty() {
        return Err(format!(
            "`{base}` declares no required checks, so no failure can be scoped to them"
        ));
    }
    let failed = failed_required_checks(run_gh, repo, sha, &required)?;
    if failed.is_empty() {
        return Err(format!(
            "no required check failed on merge-group commit {}, so the ejection is unexplained",
            short(sha)
        ));
    }
    let mut evidence = Vec::new();
    for check in &failed {
        let Some(job_id) = check.id.filter(|_| check.actions_job) else {
            return Err(format!(
                "required check `{}` ({}) is not a GitHub Actions job, so it has no log to read",
                check.name, check.conclusion
            ));
        };
        if check.conclusion != "failure" {
            return Err(format!(
                "required check `{}` concluded `{}`, which is not an environment signature",
                check.name, check.conclusion
            ));
        }
        evidence.extend(read_job(run_gh, repo, &check.name, job_id)?);
    }
    let unexplained = evidence
        .iter()
        .filter(|step| !matches!(step.reading, StepReading::Environment(_)))
        .map(StepEvidence::render)
        .collect::<Vec<_>>();
    let commit = Some(sha.to_owned());
    if unexplained.is_empty() {
        Ok(EnvironmentRequeue {
            allowed: true,
            reason: format!(
                "every failing required check on merge-group commit {} failed on the network, \
                 and this is the head's first ejection",
                short(sha)
            ),
            merge_group_commit: commit,
            evidence,
        })
    } else {
        Ok(EnvironmentRequeue {
            allowed: false,
            reason: format!(
                "a required failure on merge-group commit {} is not an environment failure: {}",
                short(sha),
                unexplained.join("; ")
            ),
            merge_group_commit: commit,
            evidence,
        })
    }
}

fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

#[cfg(test)]
mod tests;
