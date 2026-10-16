//! Naming a red job C2 could only call a bare exit.
//!
//! A summary job (Pulp's `linux` or `macos`, whose one step checks the
//! results of the jobs it needs) fails only because a job it summarises
//! failed, so it takes that job's cause and says where it came from. Any
//! other bare exit is labelled by the last line its step printed (the
//! signature; the step stays in `failed_step`), read only
//! between the step's `##[endgroup]` and its `##[error]`:
//! the lines before that are the step's script, echoed by the runner, and
//! name nothing. A label is never more than `low` confidence.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Value, json};

use crate::diagnose::c2;
use crate::diagnose::cause::{self, CiCause};
use crate::diagnose::evidence::split_lines;

const EXIT_LINE: &str = "##[error]Process completed with exit code";
/// Lines of excerpt kept around a label's line.
const LABEL_EXCERPT: usize = 3;
/// Furthest back from the exit a label's line is looked for.
const LABEL_REACH: usize = 400;

static ERRORISH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:error|errors|fail|failed|failure|fatal|notrun|not run|denied|missing|refused|timed out|timeout|stale|drift|abort|aborted|panic|panicked|exception|invalid|cannot|could not|unable)\b|[✗✖❌]",
    )
    .expect("errorish pattern compiles")
});

/// Label a bare exit by its step and the last line the step printed. Returns
/// the label kind for the row (`step` when a printed line was found,
/// `step_no_output` when the step printed nothing after its script), or
/// `None` when the cause is not a bare exit.
pub(super) fn label_bare_exit(cause: &mut CiCause, log: Option<&str>) -> Option<&'static str> {
    if cause.format != "shell-exit" {
        return None;
    }
    let text = cause::without_records(log?);
    let lines: Vec<String> = split_lines(&text).into_iter().map(c2::strip_line).collect();
    let exit = cause
        .evidence
        .log_line
        .checked_sub(1)
        .filter(|&at| {
            lines
                .get(at)
                .is_some_and(|line| line.trim().starts_with(EXIT_LINE))
        })
        .or_else(|| {
            lines
                .iter()
                .position(|line| line.trim().starts_with(EXIT_LINE))
        })?;
    // The step's output: from its exit back to the end of its echoed script.
    let output: Vec<usize> = (exit.saturating_sub(LABEL_REACH)..exit)
        .rev()
        .take_while(|&at| {
            let line = lines[at].trim();
            !line.starts_with("##[endgroup]") && !line.starts_with("##[group]")
        })
        .filter(|&at| {
            let line = lines[at].trim();
            !line.is_empty() && !line.starts_with("##[")
        })
        .collect();
    let Some(&at) = output
        .iter()
        .find(|&&at| ERRORISH.is_match(&lines[at]))
        .or_else(|| output.first())
    else {
        cause.evidence.excerpt.clear();
        return Some("step_no_output");
    };
    // The step stays in `failed_step`; the line it printed names the failure.
    cause.signature = c2::norm(lines[at].trim(), 160);
    cause.detail = Some(c2::norm(lines[at].trim(), 200));
    cause.evidence.log_line = at + 1;
    let mut excerpt: Vec<String> = output
        .iter()
        .filter(|&&line| line <= at)
        .take(LABEL_EXCERPT)
        .map(|&line| c2::norm(lines[line].trim(), 200))
        .collect();
    excerpt.reverse();
    cause.evidence.excerpt = excerpt;
    Some("step")
}

fn named(row: &Value) -> bool {
    row["failure_class"] != "unknown"
        && matches!(row["confidence"].as_str(), Some("high" | "medium"))
}

/// Whether `job` is a job `summary` summarises: its name is the summary's
/// name followed by a space, `(` or `[` (`macos` summarises
/// `macOS (ARM64) [local]`, not `macos-merge-group`).
fn summarises(summary: &str, job: &str) -> bool {
    let (summary, job) = (summary.to_lowercase(), job.to_lowercase());
    job.strip_prefix(&summary)
        .and_then(|rest| rest.chars().next())
        .is_some_and(|next| matches!(next, ' ' | '(' | '['))
}

/// A bare-exit row in `rows` takes the cause of the named job of the same run
/// attempt it summarises, found in `rows` or `sources`. Returns how many rows
/// inherited.
pub(super) fn inherit_from_summarised(rows: &mut [Value], sources: &[Value]) -> usize {
    let mut inherited = 0;
    for index in 0..rows.len() {
        let row = &rows[index];
        if row["format"] != "shell-exit" {
            continue;
        }
        let (run, attempt, job_id) = (&row["run_id"], &row["attempt"], &row["job_id"]);
        let summary = row["job"].as_str().unwrap_or_default().to_owned();
        let source = rows
            .iter()
            .chain(sources)
            .filter(|other| {
                &other["run_id"] == run
                    && &other["attempt"] == attempt
                    && &other["job_id"] != job_id
                    && named(other)
                    && summarises(&summary, other["job"].as_str().unwrap_or_default())
            })
            .min_by_key(|other| other["job_id"].as_u64().unwrap_or(u64::MAX))
            .cloned();
        let Some(source) = source else {
            continue;
        };
        let fields = rows[index].as_object_mut().expect("cause rows are objects");
        for key in [
            "failure_class",
            "format",
            "signature",
            "detail",
            "confidence",
            "fix_hint",
            "kind",
            "location",
            // The evidence is the source job's, so it describes the same log
            // line as the detail; evidence_job_id names whose log that is.
            "evidence",
            "step_failed",
            // The step named is the source job's, the step the cause is in.
            "failed_step",
        ] {
            fields.insert(key.to_owned(), source[key].clone());
        }
        fields.insert("inherited_from_job_id".to_owned(), source["job_id"].clone());
        fields.insert("evidence_job_id".to_owned(), source["job_id"].clone());
        fields.insert("inherited_from_job".to_owned(), source["job"].clone());
        fields.insert("labelled_by".to_owned(), json!("summarised_job"));
        inherited += 1;
    }
    inherited
}

#[cfg(test)]
mod tests;
