//! `CTest` results read out of a GitHub Actions job log.
//!
//! A job log is one text stream for every step. Each `run` step opens with a
//! `##[group]Run <script>` header, and every line carries the runner's
//! timestamp, so [`step_sections`] maps headers to the steps the jobs API
//! lists by their start times. Within the lines of chosen steps,
//! [`results`] reads `CTest`'s per-test result lines, and [`failed_tests`]
//! every "The following tests FAILED" block.
//!
//! `ctest --repeat until-pass:N` prints a retry's result line without the
//! `k/N` progress prefix; [`CtestResult::retry`] marks those, so a test that
//! failed and then passed on the same build reads as both outcomes.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde_json::Value;

use crate::diagnose::Step;
use crate::diagnose::evidence::{self, CtestFailure};

/// One `CTest` result line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtestResult {
    /// Test name.
    pub name: String,
    /// `Passed`, `Failed`, `Skipped`, `Timeout`, `Not Run`, ...: the result
    /// text without `CTest`'s `***` marker.
    pub outcome: String,
    /// A `--repeat` retry: the line has no `k/N` progress prefix.
    pub retry: bool,
}

impl CtestResult {
    /// The test passed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.outcome == "Passed"
    }

    /// The test ran and did not pass: anything but `Passed`, a skip, or a
    /// disabled test (`Not Run (Disabled)`).
    #[must_use]
    pub fn failed(&self) -> bool {
        !self.passed() && !self.outcome.starts_with("Skipped") && !self.outcome.contains("Disabled")
    }
}

static RESULT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^\s*(\d+/\d+\s+)?Test\s+#\d+: (.+) \.{3,}\s*(?:\*{3})?(\S.*?)\s+[\d.]+ sec\s*$")
        .expect("ctest result pattern compiles")
});

static HEADER_TIME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d)(?:\.\d+)?Z ##\[group\]Run ")
        .expect("step header pattern compiles")
});

/// Every `CTest` result line in `lines` (timestamps already removed), in
/// log order.
#[must_use]
pub fn results(lines: &[String]) -> Vec<CtestResult> {
    lines
        .iter()
        .filter_map(|line| {
            let caps = RESULT.captures(line)?;
            Some(CtestResult {
                name: caps[2].trim().to_owned(),
                outcome: caps[3].trim().to_owned(),
                retry: caps.get(1).is_none(),
            })
        })
        .collect()
}

/// The entries of every "The following tests FAILED" block in `lines`,
/// first occurrence of each name kept.
#[must_use]
pub fn failed_tests(lines: &[String]) -> Vec<CtestFailure> {
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("The following tests FAILED"))
        .map(|(index, _)| index)
        .collect();
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (position, start) in starts.iter().enumerate() {
        let end = starts.get(position + 1).copied().unwrap_or(lines.len());
        for failure in evidence::ctest_failures(lines, *start, end) {
            if seen.insert(failure.name.clone()) {
                out.push(failure);
            }
        }
    }
    out
}

/// A log split into lines, and the same lines without timestamps or ANSI
/// escapes.
pub struct JobLog<'a> {
    raw: Vec<&'a str>,
    clean: Vec<String>,
}

impl<'a> JobLog<'a> {
    /// Split `text` the way the diagnosis does.
    #[must_use]
    pub fn new(text: &'a str) -> Self {
        let raw = evidence::split_lines(text);
        let clean = evidence::clean(&raw);
        Self { raw, clean }
    }

    /// Every line, cleaned.
    #[must_use]
    pub fn lines(&self) -> &[String] {
        &self.clean
    }

    /// The cleaned lines of the steps named in `wanted`, in log order, and
    /// the names of those steps a section was found for.
    #[must_use]
    pub fn lines_of(&self, steps: &[Step], wanted: &[String]) -> (Vec<String>, BTreeSet<String>) {
        let mut out = Vec::new();
        let mut found = BTreeSet::new();
        for (lo, hi, name) in step_sections(&self.raw, steps) {
            if let Some(name) = name.filter(|name| wanted.contains(name)) {
                out.extend_from_slice(&self.clean[lo..hi]);
                found.insert(name);
            }
        }
        (out, found)
    }

    /// The cleaned lines of every section NOT belonging to a step in
    /// `wanted`.
    #[must_use]
    pub fn lines_outside(&self, steps: &[Step], wanted: &[String]) -> Vec<String> {
        let mut out = Vec::new();
        let mut covered = 0;
        for (lo, hi, name) in step_sections(&self.raw, steps) {
            out.extend_from_slice(&self.clean[covered..lo]);
            if !name.as_ref().is_some_and(|name| wanted.contains(name)) {
                out.extend_from_slice(&self.clean[lo..hi]);
            }
            covered = hi;
        }
        out.extend_from_slice(&self.clean[covered..]);
        out
    }
}

/// The steps of one job from the jobs API, in step order.
#[must_use]
pub fn job_steps(job: &Value) -> Vec<Step> {
    let mut steps: Vec<Step> = job
        .get("steps")
        .and_then(Value::as_array)
        .map(|steps| {
            steps
                .iter()
                .filter_map(|step| serde_json::from_value(step.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    steps.sort_by_key(|step| step.number.unwrap_or(u64::MAX));
    steps
}

fn second(text: Option<&str>) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text?)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

/// A step that ran, with its start and completion second.
type RanStep<'a> = (&'a Step, DateTime<Utc>, Option<DateTime<Utc>>);

/// Steps that never open a `##[group]Run` header of their own.
fn writes_no_header(name: &str) -> bool {
    name == "Set up job" || name == "Complete job" || name.starts_with("Post ")
}

/// `[lo, hi)` line ranges, one per `##[group]Run` header, each with the step
/// it belongs to.
///
/// Skipped steps write nothing, and the runner's own set-up, completion and
/// post steps open no `Run` header, so all of those are left out. Walking
/// headers in log order, each header moves to the next step once that step
/// has started by the header's second, and keeps moving past any step that
/// had completed before that second (it cannot own a header written after
/// it ended). Otherwise the header stays with the current step: a composite
/// action writes several. A step's timestamps are whole seconds, so steps
/// that start in the same second are told apart by their headers' order. A
/// header before any step maps to `None`.
#[must_use]
pub fn step_sections(raw: &[&str], steps: &[Step]) -> Vec<(usize, usize, Option<String>)> {
    let ran: Vec<RanStep<'_>> = steps
        .iter()
        .filter(|step| step.conclusion.as_deref() != Some("skipped"))
        .filter(|step| !writes_no_header(&step.name))
        .filter_map(|step| {
            Some((
                step,
                second(step.started_at.as_deref())?,
                second(step.completed_at.as_deref()),
            ))
        })
        .collect();
    let headers: Vec<(usize, Option<DateTime<Utc>>)> = raw
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            let caps = HEADER_TIME.captures(line)?;
            let at = DateTime::parse_from_rfc3339(&format!("{}Z", &caps[1]))
                .ok()
                .map(|time| time.with_timezone(&Utc));
            Some((index, at))
        })
        .collect();
    let ended_before = |step: usize, at: DateTime<Utc>| ran[step].2.is_some_and(|end| end < at);
    let mut sections = Vec::new();
    let mut current: Option<usize> = None;
    let mut owns_header = false;
    for (position, (index, at)) in headers.iter().enumerate() {
        let end = headers
            .get(position + 1)
            .map_or(raw.len(), |(next, _)| *next);
        if let Some(at) = *at {
            loop {
                let next = current.map_or(0, |step| step + 1);
                let Some((_, started, _)) = ran.get(next) else {
                    break;
                };
                let movable = current.is_none_or(|step| owns_header || ended_before(step, at));
                if *started > at || !movable {
                    break;
                }
                current = Some(next);
                owns_header = false;
                if !ended_before(next, at) {
                    break;
                }
            }
        }
        if current.is_some() {
            owns_header = true;
        }
        sections.push((*index, end, current.map(|step| ran[step].0.name.clone())));
    }
    sections
}

#[cfg(test)]
mod tests;
