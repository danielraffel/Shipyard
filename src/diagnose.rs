//! Bounded CI failure diagnosis (`shipyard.diagnose/v1`).
//!
//! For each failing REQUIRED job of a workflow run this names the failing
//! step, the failing tests, a few groups of evidence lines, the runner, a
//! classification and a pointer to the full log, in a document whose
//! serialized size never exceeds a byte cap whatever the size of the logs.
//! An agent reads the diagnosis instead of a 10 MB job log.
//!
//! Everything here is pure: the caller fetches the jobs payload, each failing
//! job's log and each cancelled job's check-run annotations, and passes them
//! in. [`annotate`] turns a diagnosis into check-run annotations that put each
//! failing test's evidence on the file and line it names.
//!
//! The classes and their rules live in [`classify`]; the evidence extraction
//! in [`evidence`]. A classification acts on nothing by itself: `infra` and
//! `interrupted` describe why a red is not a code failure, `flake_candidate`
//! is a hint that never exonerates, `stale_base` says the head needs main.

use std::collections::HashMap;
use std::fmt::Write as _;

use regex::Regex;
use serde::{Deserialize, Serialize};

pub mod annotate;
pub mod classify;
pub mod evidence;

pub use classify::Classification;

/// Schema identifier of the diagnosis document.
pub const SCHEMA: &str = "shipyard.diagnose/v1";
/// Default cap on the serialized diagnosis, in bytes.
pub const DEFAULT_MAX_BYTES: usize = 16_384;
/// At most this many failing required checks are diagnosed; the rest are named
/// in `omitted_checks`.
pub const MAX_CHECKS: usize = 8;
/// At most this many failing test names per check.
pub const MAX_TESTS: usize = 20;
/// At most this many evidence groups per check.
pub const MAX_GROUPS: usize = 5;
/// At most this many passed-on-retry test names per check.
pub const MAX_RETRIED: usize = 5;

/// One step of a workflow job, as the jobs API returns it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Step {
    /// Step number within the job.
    #[serde(default)]
    pub number: Option<u64>,
    /// Display name.
    #[serde(default)]
    pub name: String,
    /// `success`, `failure`, `cancelled`, `skipped`, `timed_out`, or null.
    #[serde(default)]
    pub conclusion: Option<String>,
    /// RFC 3339 start.
    #[serde(default)]
    pub started_at: Option<String>,
    /// RFC 3339 end.
    #[serde(default)]
    pub completed_at: Option<String>,
}

/// One workflow job, as `GET /repos/{o}/{r}/actions/runs/{id}/jobs` returns it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Job {
    /// Job id; also the id of its check run.
    #[serde(default)]
    pub id: i64,
    /// Job name; also its check-run name.
    #[serde(default)]
    pub name: String,
    /// Final conclusion, null while running.
    #[serde(default)]
    pub conclusion: Option<String>,
    /// `queued`, `in_progress`, `completed`.
    #[serde(default)]
    pub status: Option<String>,
    /// Runner display name; empty or null when no runner took the job.
    #[serde(default)]
    pub runner_name: Option<String>,
    /// Requested runner labels.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// RFC 3339 creation.
    #[serde(default)]
    pub created_at: Option<String>,
    /// RFC 3339 start.
    #[serde(default)]
    pub started_at: Option<String>,
    /// RFC 3339 completion.
    #[serde(default)]
    pub completed_at: Option<String>,
    /// Owning workflow run.
    #[serde(default)]
    pub run_id: Option<u64>,
    /// Run attempt.
    #[serde(default)]
    pub run_attempt: Option<u64>,
    /// Browser URL of the job.
    #[serde(default)]
    pub html_url: Option<String>,
    /// Commit the job ran on.
    #[serde(default)]
    pub head_sha: Option<String>,
    /// Branch of the run (`gh-readonly-queue/<base>/pr-<n>-<sha>` for a merge group).
    #[serde(default)]
    pub head_branch: Option<String>,
    /// Steps; null or empty when the job never ran.
    #[serde(default)]
    pub steps: Option<Vec<Step>>,
}

impl Job {
    /// The job's steps, empty when none were recorded.
    #[must_use]
    pub fn steps(&self) -> &[Step] {
        self.steps.as_deref().unwrap_or(&[])
    }

    fn concluded(&self, value: &str) -> bool {
        self.conclusion.as_deref() == Some(value)
    }

    /// Whether the job failed, was cancelled, or timed out.
    #[must_use]
    pub fn is_bad(&self) -> bool {
        matches!(
            self.conclusion.as_deref(),
            Some("failure" | "cancelled" | "timed_out")
        )
    }

    /// Cancelled with no runner and no steps: never ran at all.
    #[must_use]
    pub fn runnerless(&self) -> bool {
        self.concluded("cancelled")
            && self.runner_name.as_deref().unwrap_or("").is_empty()
            && self.steps().is_empty()
    }

    /// The first step that failed, was cancelled or timed out.
    #[must_use]
    pub fn failing_step(&self) -> Option<&Step> {
        self.steps().iter().find(|step| {
            matches!(
                step.conclusion.as_deref(),
                Some("failure" | "cancelled" | "timed_out")
            )
        })
    }
}

/// Inputs that are not the jobs or their logs.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Test name to other pull requests that failed it recently (pr-watch's
    /// shared-failure flags). Empty means no corroboration is known.
    pub history: HashMap<String, Vec<u64>>,
    /// Lines that mean the head is behind the protected base (config
    /// `diagnose.stale_base_markers`).
    pub stale_markers: Vec<Regex>,
    /// Lines that mean a gate failed closed on a dependency (default plus
    /// config `diagnose.fail_closed_markers`).
    pub fail_closed: Vec<Regex>,
    /// Check-run annotation messages by job id (only cancelled jobs need them).
    pub annotations: HashMap<i64, Vec<String>>,
}

impl Context {
    /// A context with the default fail-closed marker and nothing else.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fail_closed: classify::default_fail_closed(),
            ..Self::default()
        }
    }
}

/// Runner of a check.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Runner {
    /// Display name; null when no runner took the job.
    pub name: Option<String>,
    /// First labels the job requested.
    pub labels: Vec<String>,
}

/// Queue and run durations.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Timing {
    /// Created to started, seconds.
    pub queued: Option<i64>,
    /// Started to completed, seconds.
    pub ran: Option<i64>,
}

/// The failing step.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StepRef {
    /// Step number.
    pub number: Option<u64>,
    /// Step name.
    pub name: String,
    /// Step conclusion.
    pub conclusion: Option<String>,
}

/// A capped list of names with its true total.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Names {
    /// Names, capped.
    pub names: Vec<String>,
    /// How many there were.
    pub total: usize,
    /// Whether `names` was cut.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Failing tests carry an explicit `truncated` even when false.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailingTests {
    /// Names, capped at [`MAX_TESTS`].
    pub names: Vec<String>,
    /// How many tests failed.
    pub total: usize,
    /// Whether `names` was cut.
    pub truncated: bool,
}

/// One evidence group.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EvidenceGroup {
    /// `test` (a failing test's output), `signal` (scored error lines) or
    /// `tail` (the step's last lines before it exited).
    pub kind: String,
    /// 1-based line of the group's anchor in the job log.
    pub line: usize,
    /// The failing test this group belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    /// How many times the same group occurred (retries, repeated messages).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeats: Option<usize>,
    /// The lines, each capped.
    pub lines: Vec<String>,
}

/// Where the full log is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct More {
    /// Browser URL of the job.
    pub job_url: Option<String>,
    /// Lines in the job log.
    pub log_lines: Option<usize>,
    /// Bytes in the job log.
    pub log_bytes: Option<usize>,
    /// 1-based first and last line of the failing step in the log.
    pub step_lines: Option<[usize; 2]>,
    /// Command that prints the full log.
    pub fetch: String,
    /// Why there is no log, when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// The diagnosis of one failing required check.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckDiagnosis {
    /// Required context (the job name).
    pub context: String,
    /// Job conclusion.
    pub conclusion: Option<String>,
    /// Workflow run.
    pub run_id: Option<u64>,
    /// Run attempt.
    pub run_attempt: Option<u64>,
    /// Job (and check-run) id.
    pub job_id: i64,
    /// Runner.
    pub runner: Runner,
    /// Durations.
    pub timing_s: Timing,
    /// Failing step.
    pub failing_step: Option<StepRef>,
    /// Class, rule and reason.
    pub classification: Classification,
    /// Failing tests.
    pub failing_tests: FailingTests,
    /// Tests that failed an attempt in this step and passed on retry.
    pub passed_on_retry: Option<Names>,
    /// Evidence groups, most decisive first.
    pub evidence: Vec<EvidenceGroup>,
    /// Whether groups were dropped.
    pub evidence_truncated: bool,
    /// Pointer to the full log.
    pub more: More,
}

/// Failing jobs that are not required.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Advisory {
    /// How many.
    pub count: usize,
    /// First names.
    pub names: Vec<String>,
}

/// A required check that could not be diagnosed, and why.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Unreadable {
    /// Context name.
    pub context: String,
    /// Why.
    pub reason: String,
}

/// Size accounting of the serialized document.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Bounds {
    /// The cap.
    pub max_bytes: usize,
    /// Whether anything was cut to fit.
    pub truncated: bool,
    /// Exact compact-serialized size of the whole document.
    pub bytes: usize,
}

/// The diagnosis document.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Diagnosis {
    /// [`SCHEMA`].
    pub schema: String,
    /// `red`, `green` or `pending`.
    pub verdict: String,
    /// One line.
    pub summary: String,
    /// Failing required checks.
    pub checks: Vec<CheckDiagnosis>,
    /// Failing required checks not diagnosed (over [`MAX_CHECKS`] or the cap).
    pub omitted_checks: Vec<String>,
    /// Failing non-required jobs.
    pub advisory_failures: Advisory,
    /// Required checks that could not be read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unreadable_checks: Vec<Unreadable>,
    /// GitHub reads spent producing this document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_calls: Option<u32>,
    /// Size accounting.
    pub bounds: Bounds,
}

impl Diagnosis {
    /// Compact serialization, exactly what `bounds.bytes` measures.
    #[must_use]
    pub fn to_compact(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Diagnose every failing required job in `jobs` (one or several runs), with
/// its log from `logs` (keyed by job id), capped at `max_bytes`.
#[must_use]
pub fn build<S: std::hash::BuildHasher>(
    jobs: &[Job],
    logs: &HashMap<i64, String, S>,
    required: &[String],
    context: &Context,
    max_bytes: usize,
) -> Diagnosis {
    let is_required = |job: &Job| required.iter().any(|name| name == &job.name);
    let bad: Vec<&Job> = jobs.iter().filter(|job| job.is_bad()).collect();
    let gate: Vec<&Job> = bad.iter().copied().filter(|job| is_required(job)).collect();
    let advisory: Vec<String> = bad
        .iter()
        .filter(|job| !is_required(job))
        .map(|job| job.name.clone())
        .collect();
    let checks: Vec<CheckDiagnosis> = gate
        .iter()
        .take(MAX_CHECKS)
        .map(|job| diagnose_job(job, jobs, logs.get(&job.id).map(String::as_str), context))
        .collect();
    let pending = jobs
        .iter()
        .filter(|job| is_required(job))
        .any(|job| job.status.as_deref() != Some("completed"));
    let verdict = match (checks.is_empty(), pending) {
        (false, _) => "red",
        (true, true) => "pending",
        (true, false) => "green",
    };
    let mut doc = Diagnosis {
        schema: SCHEMA.to_owned(),
        verdict: verdict.to_owned(),
        summary: summarize(&checks),
        checks,
        omitted_checks: gate
            .iter()
            .skip(MAX_CHECKS)
            .map(|job| job.name.clone())
            .collect(),
        advisory_failures: Advisory {
            count: advisory.len(),
            names: advisory.into_iter().take(5).collect(),
        },
        unreadable_checks: Vec::new(),
        api_calls: None,
        bounds: Bounds {
            max_bytes,
            truncated: false,
            bytes: 0,
        },
    };
    fit(&mut doc, max_bytes);
    doc
}

/// Seconds between two RFC 3339 instants.
#[must_use]
pub fn seconds(from: Option<&str>, to: Option<&str>) -> Option<i64> {
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    Some((parse(to?)? - parse(from?)?).num_seconds())
}

fn diagnose_job(
    job: &Job,
    siblings: &[Job],
    log: Option<&str>,
    context: &Context,
) -> CheckDiagnosis {
    let step = job.failing_step();
    let log = log.filter(|text| !text.is_empty());
    let raw: Vec<&str> = log.map(evidence::split_lines).unwrap_or_default();
    let lines: Vec<String> = evidence::clean(&raw);
    let (lo, hi) = if raw.is_empty() {
        (0, 0)
    } else {
        evidence::step_window(&raw, step)
    };
    let found = evidence::extract(&lines, lo, hi);
    let classification =
        classify::classify(job, siblings, step, &found.tests, &lines[lo..hi], context);
    let total = found.tests.len();
    let groups = found.groups;
    CheckDiagnosis {
        context: job.name.clone(),
        conclusion: job.conclusion.clone(),
        run_id: job.run_id,
        run_attempt: job.run_attempt,
        job_id: job.id,
        runner: Runner {
            name: job.runner_name.clone().filter(|name| !name.is_empty()),
            labels: job
                .labels
                .clone()
                .unwrap_or_default()
                .into_iter()
                .take(4)
                .collect(),
        },
        timing_s: Timing {
            queued: seconds(job.created_at.as_deref(), job.started_at.as_deref()),
            ran: seconds(job.started_at.as_deref(), job.completed_at.as_deref()),
        },
        failing_step: step.map(|step| StepRef {
            number: step.number,
            name: step.name.clone(),
            conclusion: step.conclusion.clone(),
        }),
        classification,
        failing_tests: FailingTests {
            names: found
                .tests
                .iter()
                .take(MAX_TESTS)
                .map(|name| evidence::clip(name.as_str()))
                .collect(),
            total,
            truncated: total > MAX_TESTS,
        },
        passed_on_retry: (!found.retried.is_empty()).then(|| Names {
            names: found
                .retried
                .iter()
                .take(MAX_RETRIED)
                .map(|name| evidence::clip(name.as_str()))
                .collect(),
            total: found.retried.len(),
            truncated: false,
        }),
        evidence_truncated: groups.len() > MAX_GROUPS,
        evidence: groups
            .into_iter()
            .take(MAX_GROUPS)
            .map(|group| EvidenceGroup {
                kind: group.kind.to_owned(),
                line: group.anchor,
                test: group.test,
                repeats: (group.repeats > 1).then_some(group.repeats),
                lines: group.lines,
            })
            .collect(),
        more: More {
            job_url: job.html_url.clone(),
            log_lines: (!raw.is_empty()).then_some(raw.len()),
            log_bytes: log.map(str::len),
            step_lines: (!raw.is_empty()).then_some([lo + 1, hi]),
            fetch: format!(
                "gh run view {} --job {} --log",
                job.run_id
                    .map_or_else(|| "None".to_owned(), |id| id.to_string()),
                job.id
            ),
            note: log
                .is_none()
                .then(|| "no job log in the run archive".to_owned()),
        },
    }
}

/// The one-line summary.
#[must_use]
pub fn summarize(checks: &[CheckDiagnosis]) -> String {
    if checks.is_empty() {
        return "no required check failed".to_owned();
    }
    let parts: Vec<String> = checks
        .iter()
        .map(|check| {
            let mut part = format!("{}: {}", check.context, check.classification.class);
            let total = check.failing_tests.total;
            if total > 0 {
                let plural = if total == 1 { "" } else { "s" };
                let _ = write!(part, ", {total} test{plural}");
            }
            if let Some(step) = &check.failing_step {
                let _ = write!(part, ", step '{}'", step.name);
            }
            part
        })
        .collect();
    format!(
        "{} required check(s) failed. {}",
        checks.len(),
        parts.join("; ")
    )
}

/// Cut the document until its compact serialization, the `bounds.bytes` field
/// included, fits `limit`. Cuts, in order, from the largest check: halve its
/// longest evidence group (to no fewer than three lines), drop its last group,
/// drop its passed-on-retry list, drop its last test name, then move the whole
/// check into `omitted_checks`. When nothing is left to cut the skeleton is
/// returned as is.
pub fn fit(doc: &mut Diagnosis, limit: usize) {
    let mut cuts = 0_usize;
    loop {
        doc.bounds.truncated = cuts > 0;
        // The count's own digits change the size it reports, so measure until
        // the reported size is the size.
        doc.bounds.bytes = 0;
        for _ in 0..4 {
            let size = doc.to_compact().len();
            if size == doc.bounds.bytes {
                break;
            }
            doc.bounds.bytes = size;
        }
        if doc.bounds.bytes <= limit {
            return;
        }
        cuts += 1;
        let Some(index) = largest_check(&doc.checks) else {
            return;
        };
        let check = &mut doc.checks[index];
        if !check.evidence.is_empty() {
            check.evidence_truncated = true;
            let longest = longest_group(&check.evidence);
            let group = &mut check.evidence[longest];
            if group.lines.len() > 3 {
                let keep = (group.lines.len() / 2).max(3);
                group.lines.truncate(keep);
            } else {
                check.evidence.pop();
            }
            continue;
        }
        if check.passed_on_retry.is_some() {
            check.passed_on_retry = None;
            continue;
        }
        if !check.failing_tests.names.is_empty() {
            check.failing_tests.names.pop();
            check.failing_tests.truncated = true;
            continue;
        }
        let dropped = doc.checks.remove(index);
        doc.omitted_checks.push(dropped.context);
    }
}

fn largest_check(checks: &[CheckDiagnosis]) -> Option<usize> {
    let mut best: Option<(usize, usize)> = None;
    for (index, check) in checks.iter().enumerate() {
        let size = serde_json::to_string(check).map_or(0, |text| text.len());
        if best.is_none_or(|(_, top)| size > top) {
            best = Some((index, size));
        }
    }
    best.map(|(index, _)| index)
}

fn longest_group(groups: &[EvidenceGroup]) -> usize {
    let mut best = 0;
    for (index, group) in groups.iter().enumerate() {
        if group.lines.len() > groups[best].lines.len() {
            best = index;
        }
    }
    best
}

#[cfg(test)]
mod tests;
