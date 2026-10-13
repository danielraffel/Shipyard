//! `ci.cause/v1`: the cause of one failed job, as one record.
//!
//! The record is EXP-A3's `to_ci_cause_v1` mapping of the C2 record
//! ([`super::c2::Record`]) chosen as the job's cause: where the job ran
//! (`repo`, `run_id`, `attempt`, `job_id`, `job`, `head_sha`, `event`, `pr`,
//! `failed_step`), what failed (`failure_class`, `format`, `signature`,
//! `detail`, `evidence`), and how sure and who said so (`confidence`,
//! `source`, `extractor`, `named_at`, `supersedes`). It also keeps the C2
//! fields that mapping drops (`kind`, `location`, `step_failed`), so it is a
//! superset of `c2.cause.v0`, and `retrospective`.
//!
//! C2 names the cause: its first record, which comes from the step that
//! ended in `##[error]`. `diagnose` contributes only what a log cannot say,
//! GitHub's own record of a runner that never came, a supersede or a
//! timeout, used when it is more certain than anything in the log.
//!
//! A cause is named when its class is not `unknown` and its confidence is
//! `high` or `medium`.

use std::collections::HashMap;
use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::c2::{self, Record};
use super::classify::{INFRA, INTERRUPTED};
use super::evidence::split_lines;
use super::{CheckDiagnosis, Context, Job};

/// Record schema identifier.
pub const SCHEMA: &str = "ci.cause/v1";
/// The extractor that named a record built here.
pub const EXTRACTOR: &str = concat!("c2@shipyard-", env!("CARGO_PKG_VERSION"));
/// Who named a record built here.
pub const SOURCE_EXTRACTOR: &str = "extractor";
/// Most evidence lines a record keeps.
pub const MAX_EXCERPT: usize = 12;
/// Longest evidence line.
pub const MAX_EXCERPT_CHARS: usize = 240;
/// Every failure class.
pub const CLASSES: [&str; 8] = [
    "test",
    "build",
    "infra",
    "cancelled",
    "timeout",
    "drift",
    "gate_policy",
    "unknown",
];

/// Where in the log the cause is.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// 1-based line in the job log; 0 when the cause is not a log line.
    pub log_line: usize,
    /// The log around it, normalized.
    pub excerpt: Vec<String>,
}

/// The cause of one failed job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CiCause {
    /// [`SCHEMA`].
    pub schema: String,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Workflow run.
    pub run_id: Option<u64>,
    /// Run attempt.
    pub attempt: Option<u64>,
    /// Job (check-run) id.
    pub job_id: Option<u64>,
    /// Job name.
    pub job: String,
    /// Commit the job ran on.
    pub head_sha: Option<String>,
    /// Workflow-run event, when known.
    pub event: Option<String>,
    /// Pull request, when known.
    pub pr: Option<u64>,
    /// The step that failed.
    pub failed_step: Option<String>,
    /// One of [`CLASSES`].
    pub failure_class: String,
    /// The format id (`ctest`, `catch2`, `compiler`, `infra-runner`, ...).
    pub format: String,
    /// The most specific stable name for the cause (C2's `subject`).
    pub signature: String,
    /// Its first detail line (C2's `message`), when it has one.
    pub detail: Option<String>,
    /// Where in the log.
    pub evidence: Evidence,
    /// How to fix it, when known.
    pub fix_hint: Option<String>,
    /// `high`, `medium` or `low`.
    pub confidence: String,
    /// `extractor`, `human`, `agent` or `fix-link`.
    pub source: String,
    /// Who named it: [`EXTRACTOR`] for an extractor record.
    pub extractor: String,
    /// When the cause was named.
    pub named_at: Option<String>,
    /// The record this one replaces.
    pub supersedes: Option<String>,
    /// C2's kind.
    pub kind: String,
    /// C2's `file[:line]`.
    pub location: String,
    /// Whether the cause's line sits in a step that ended in `##[error]`.
    pub step_failed: Option<bool>,
    /// Named after the fact from a stored log, rather than when the job ended.
    pub retrospective: bool,
}

impl CiCause {
    /// Whether the cause counts as named: a class other than `unknown`, at
    /// `high` or `medium` confidence.
    #[must_use]
    pub fn named(&self) -> bool {
        self.failure_class != "unknown" && matches!(self.confidence.as_str(), "high" | "medium")
    }

    /// Stable id: the job attempt and who named it.
    #[must_use]
    pub fn id(&self) -> String {
        let part = |value: Option<u64>| value.map_or_else(|| "-".to_owned(), |n| n.to_string());
        format!(
            "{}/{}/{}/{}/{}",
            self.repo,
            part(self.run_id),
            part(self.attempt),
            part(self.job_id),
            self.source
        )
    }
}

/// C2 format to `ci.cause/v1` format and failure class.
#[must_use]
pub fn v1(format: &str) -> (&'static str, &'static str) {
    match format {
        "ctest_summary" | "ctest_inline" => ("ctest", "test"),
        "pulp_affected" => ("pulp-affected", "test"),
        "unittest" => ("unittest", "test"),
        "catch2" => ("catch2", "test"),
        "cargo_test" => ("cargo-test", "test"),
        "rust_panic" => ("rust-panic", "test"),
        "rust_compile" => ("rustc", "build"),
        "cargo_build" => ("cargo-build", "build"),
        "node_test" => ("node-test", "test"),
        "pytest" => ("pytest", "test"),
        "python_traceback" => ("py-exception", "test"),
        "clang" | "linker" => ("compiler", "build"),
        "cmake" => ("cmake", "build"),
        "ninja" => ("ninja", "build"),
        "gate_policy" => ("gate-sync", "gate_policy"),
        "infra" => ("infra-runner", "infra"),
        "git_push" => ("git-push", "drift"),
        "github_api" => ("github-api", "drift"),
        "merge_queue" => ("merge-queue", "unknown"),
        "log_missing" => ("log-missing", "unknown"),
        _ => ("shell-exit", "unknown"),
    }
}

/// How sure a C2 record is: `low` for an exit code, a bare annotation, a
/// bare cancel, or a line outside the failing step; `medium` for formats that can name a
/// symptom rather than the failure; else `high`.
#[must_use]
pub fn confidence(record: &Record) -> &'static str {
    if matches!(record.format.as_str(), "gha_annotation" | "shell_exit")
        || record.step_failed == Some(false)
        // A bare cancel says nothing of who cancelled or why; it is named only
        // once the cancel is attributed (a supersede, a timeout).
        || (record.format == "infra" && record.subject == "operation canceled")
    {
        "low"
    } else if matches!(
        record.format.as_str(),
        "python_traceback" | "ninja" | "linker" | "catch2"
    ) {
        "medium"
    } else {
        "high"
    }
}

/// Where a job ran.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Place {
    /// `OWNER/REPO`.
    pub repo: String,
    /// Workflow run.
    pub run_id: Option<u64>,
    /// Run attempt.
    pub attempt: Option<u64>,
    /// Job (check-run) id.
    pub job_id: Option<u64>,
    /// Job name.
    pub job: String,
    /// Commit.
    pub head_sha: Option<String>,
    /// Workflow-run event.
    pub event: Option<String>,
    /// Pull request.
    pub pr: Option<u64>,
    /// The step that failed.
    pub failed_step: Option<String>,
}

/// EXP-A3's `to_ci_cause_v1`: one C2 record as a `ci.cause/v1` record.
/// `lines` are the raw log lines, for the evidence excerpt.
#[must_use]
pub fn to_ci_cause_v1(
    record: &Record,
    place: &Place,
    lines: &[&str],
    named_at: Option<String>,
    retrospective: bool,
) -> CiCause {
    let (mut format, mut class) = v1(&record.format);
    if record.format == "infra" {
        class = c2::infra_class(&record.subject);
        format = if class == "cancelled" {
            "infra-cancel"
        } else if record.subject.starts_with('E') {
            "net-infra"
        } else {
            "infra-runner"
        };
    }
    if record.format == "gate_policy" && record.message.contains("gate closed") {
        format = "pulp-gate-closed";
    }
    if record.format == "ctest_inline" && record.message.starts_with("Timeout") {
        class = "timeout";
    }
    let excerpt = if record.line >= 1 {
        let lo = record.line.saturating_sub(3);
        lines
            .iter()
            .skip(lo)
            .take(MAX_EXCERPT)
            .map(|line| c2::norm(&c2::strip_line(line), MAX_EXCERPT_CHARS))
            .collect()
    } else {
        Vec::new()
    };
    CiCause {
        schema: SCHEMA.to_owned(),
        repo: place.repo.clone(),
        run_id: place.run_id,
        attempt: place.attempt,
        job_id: place.job_id,
        job: place.job.clone(),
        head_sha: place.head_sha.clone(),
        event: place.event.clone(),
        pr: place.pr,
        failed_step: place.failed_step.clone(),
        failure_class: class.to_owned(),
        format: format.to_owned(),
        signature: record.subject.clone(),
        detail: (!record.message.is_empty()).then(|| record.message.clone()),
        evidence: Evidence {
            log_line: record.line,
            excerpt,
        },
        fix_hint: None,
        confidence: confidence(record).to_owned(),
        source: SOURCE_EXTRACTOR.to_owned(),
        extractor: EXTRACTOR.to_owned(),
        named_at,
        supersedes: None,
        kind: record.kind.clone(),
        location: record.location.clone(),
        step_failed: record.step_failed,
        retrospective,
    }
}

/// GitHub's own record for a job whose log cannot say why it failed.
fn github_cause(
    check: &CheckDiagnosis,
    place: &Place,
    named_at: Option<String>,
    retrospective: bool,
) -> Option<CiCause> {
    let classification = &check.classification;
    let (class, confidence, format) =
        match (classification.class.as_str(), classification.rule.as_str()) {
            (INFRA, _) => ("infra", "high", "infra-runner"),
            (INTERRUPTED, "timeout") => ("timeout", "high", "infra-runner"),
            (INTERRUPTED, "superseded") => ("cancelled", "high", "infra-cancel"),
            (INTERRUPTED, _) => ("cancelled", "low", "infra-cancel"),
            _ => return None,
        };
    Some(CiCause {
        schema: SCHEMA.to_owned(),
        repo: place.repo.clone(),
        run_id: place.run_id,
        attempt: place.attempt,
        job_id: place.job_id,
        job: place.job.clone(),
        head_sha: place.head_sha.clone(),
        event: place.event.clone(),
        pr: place.pr,
        failed_step: place.failed_step.clone(),
        failure_class: class.to_owned(),
        format: format.to_owned(),
        signature: classification.rule.clone(),
        detail: Some(c2::norm(&classification.why, 200)),
        evidence: Evidence::default(),
        fix_hint: None,
        confidence: confidence.to_owned(),
        source: SOURCE_EXTRACTOR.to_owned(),
        extractor: EXTRACTOR.to_owned(),
        named_at,
        supersedes: None,
        kind: classification.class.clone(),
        location: String::new(),
        step_failed: None,
        retrospective,
    })
}

fn rank(confidence: &str) -> u8 {
    match confidence {
        "high" => 3,
        "medium" => 2,
        "low" => 1,
        _ => 0,
    }
}

/// The record of a red job whose log no rule recognized.
fn unrecognized(place: &Place, named_at: Option<String>, retrospective: bool) -> CiCause {
    let record = Record {
        format: "shell_exit".to_owned(),
        kind: "exit_code".to_owned(),
        subject: String::new(),
        message: "no failing line recognized".to_owned(),
        location: String::new(),
        line: 0,
        step_failed: None,
    };
    // `shell_exit` is always `low`.
    to_ci_cause_v1(&record, place, &[], named_at, retrospective)
}

/// The cause of one job from its jobs-API payload and log (`None` when GitHub
/// holds no log for it); `None` when the job did not fail. `place` names
/// where it ran beyond what the payload says (`repo`, `event`, `pr`); the
/// payload fills the rest. A record built from a stored log is
/// `retrospective`.
#[must_use]
pub fn build(
    job: &Job,
    log: Option<&str>,
    place: &Place,
    named_at: Option<String>,
    retrospective: bool,
) -> Option<CiCause> {
    let mut logs = HashMap::new();
    if let Some(log) = log {
        logs.insert(job.id, log.to_owned());
    }
    let doc = super::build(
        std::slice::from_ref(job),
        &logs,
        std::slice::from_ref(&job.name),
        &Context::new(),
        super::DEFAULT_MAX_BYTES,
    );
    let check = doc.checks.first()?;
    let place = Place {
        repo: place.repo.clone(),
        run_id: place.run_id.or(job.run_id),
        attempt: place.attempt.or(job.run_attempt),
        job_id: place
            .job_id
            .or_else(|| u64::try_from(job.id).ok().filter(|id| *id > 0)),
        job: if place.job.is_empty() {
            job.name.clone()
        } else {
            place.job.clone()
        },
        head_sha: place.head_sha.clone().or_else(|| job.head_sha.clone()),
        event: place.event.clone().or_else(|| {
            job.head_branch
                .as_deref()
                .filter(|branch| branch.starts_with("gh-readonly-queue/"))
                .map(|_| "merge_group".to_owned())
        }),
        pr: place.pr,
        failed_step: place
            .failed_step
            .clone()
            .or_else(|| check.failing_step.as_ref().map(|step| step.name.clone())),
    };
    // A record the job printed is not evidence of its own failure: its JSON
    // quotes failing lines, which C2 would read again.
    let cleaned = log.map(without_records);
    let (records, raw) = match cleaned.as_deref() {
        Some(text) => (c2::extract(text), split_lines(text)),
        None => (c2::extract_log_fetch_error("Not Found"), Vec::new()),
    };
    let from_log = records
        .first()
        .map(|record| to_ci_cause_v1(record, &place, &raw, named_at.clone(), retrospective));
    let named_at_copy = named_at.clone();
    let github = github_cause(check, &place, named_at, retrospective);
    // GitHub's record replaces a log record that names nothing and is no
    // more certain: a missing log, an exit code, a bare cancel.
    match (from_log, github) {
        (Some(log), Some(github))
            if !log.named() && rank(&github.confidence) >= rank(&log.confidence) =>
        {
            Some(github)
        }
        (Some(log), _) => Some(log),
        (None, Some(github)) => Some(github),
        // A red job whose log holds no line C2 recognizes still gets a
        // record, so the miss is counted rather than lost.
        (None, None) => Some(unrecognized(&place, named_at_copy, retrospective)),
    }
}

/// Title of the workflow command that carries a record in a job log.
pub const NOTICE_TITLE: &str = "ci-cause";

/// The workflow command a job prints so its cause lands in its own log:
/// `::notice title=ci-cause::<record as compact JSON>`, with the data escaped
/// as GitHub's workflow commands require.
#[must_use]
pub fn notice_line(cause: &CiCause) -> String {
    let json = serde_json::to_string(cause).unwrap_or_default();
    let data = json
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A");
    format!("::notice title={NOTICE_TITLE}::{data}")
}

/// The records a job printed with [`notice_line`], as its log shows them
/// (`##[notice]<record>`, the runner having dropped the title and unescaped
/// the data). Lines that are not a `ci.cause/v1` record are ignored.
#[must_use]
pub fn records_in_log(text: &str) -> Vec<CiCause> {
    split_lines(text)
        .into_iter()
        .filter_map(|line| {
            let line = c2::strip_line(line);
            let data = line.trim().strip_prefix("##[notice]")?;
            let cause: CiCause = serde_json::from_str(data).ok()?;
            (cause.schema == SCHEMA).then_some(cause)
        })
        .collect()
}

/// `text` with every `ci.cause/v1` notice line blanked, keeping line numbers.
#[must_use]
pub fn without_records(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in split_lines(text) {
        if !records_in_log(line).is_empty() {
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Whether a record a job printed into its own log (`recorded`) may stand
/// for that job: its identity must be the payload's (`repo`, `head_sha`,
/// `run_id`, `attempt`, `job`), and its cause must be the one the extractor
/// reads from the same log (`format`, `signature`). A pull request's own code
/// runs in the job and can print any line, so a record is never taken on its
/// word: its content is the extractor's, and its time is GitHub's (see
/// [`printed_at`]), never its own `named_at`.
#[must_use]
pub fn corroborates(recorded: &CiCause, extracted: &CiCause, job: &Job, repo: &str) -> bool {
    let identity = recorded.repo == repo
        && recorded.head_sha.is_some()
        && recorded.head_sha == job.head_sha
        && recorded.run_id.is_some()
        && recorded.run_id == job.run_id
        && recorded.attempt == job.run_attempt
        && recorded.job == job.name;
    let same_cause = recorded.format == extracted.format
        && recorded.signature == extracted.signature
        && extracted.named();
    identity && same_cause
}

static LOG_TIME: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(r"^\x{feff}?(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z) ")
        .expect("log time pattern compiles")
});

/// The `ci.cause/v1` records a job printed, each with the runner's timestamp
/// on its log line.
#[must_use]
pub fn records_with_times(text: &str) -> Vec<(CiCause, Option<String>)> {
    split_lines(text)
        .into_iter()
        .filter_map(|line| {
            let cause = records_in_log(line).pop()?;
            let at = LOG_TIME.captures(line).map(|caps| caps[1].to_owned());
            Some((cause, at))
        })
        .collect()
}

/// When a record printed at log time `at` was named, by GitHub's clock: the
/// completion the jobs API records for the step whose run covers `at`. Only
/// a step that started once the failing step had completed counts: pull-request
/// code runs in earlier steps (a build) and could print a matching record
/// there, before anything failed. `None` when no such step covers `at`, or the
/// job has no failing step, so the cause stays retrospective.
#[must_use]
pub fn printed_at(job: &Job, at: Option<&str>) -> Option<String> {
    let parse = |value: &str| chrono::DateTime::parse_from_rfc3339(value).ok();
    let at = parse(at?)?;
    let failed_end = parse(job.failing_step()?.completed_at.as_deref()?)?;
    job.steps().iter().find_map(|step| {
        let start = parse(step.started_at.as_deref()?)?;
        if start < failed_end {
            return None;
        }
        let end = step.completed_at.as_deref()?;
        // The API keeps whole seconds; the log keeps fractions of one.
        let within = start <= at && at < parse(end)? + chrono::Duration::seconds(1);
        within.then(|| end.to_owned())
    })
}

/// Records as the `output.text` of a check run: one compact JSON record per
/// line under a header, never longer than `limit` characters; records that
/// do not fit are counted, not cut.
#[must_use]
pub fn check_run_text(causes: &[CiCause], limit: usize) -> Option<String> {
    if causes.is_empty() {
        return None;
    }
    let mut text = format!("{SCHEMA} records, one JSON object per line:\n");
    let mut dropped = 0;
    for cause in causes {
        let line = serde_json::to_string(cause).unwrap_or_default();
        if text.chars().count() + line.chars().count() + 64 > limit {
            dropped += 1;
            continue;
        }
        text.push_str(&line);
        text.push('\n');
    }
    if dropped > 0 {
        let _ = writeln!(text, "{dropped} record(s) did not fit");
    }
    Some(text)
}

#[cfg(test)]
mod tests;
