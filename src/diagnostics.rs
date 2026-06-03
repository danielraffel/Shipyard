//! Phase 1 failure diagnostics: GitHub workflow-job metadata + log-tail parsing.
//!
//! Issue tracking: <https://github.com/danielraffel/Shipyard/issues/303>
//!
//! The intent is to replace the lossy `Validation failed. PR #N not merged.`
//! emit with a structured block that names the failing GitHub job, its URL,
//! the failing step, and a bounded list of failing tests extracted from the
//! job log tail. We rely on Shipyard's shared `GhClient` CLI boundary, so no
//! new transport dependency is introduced. Network failure is treated as
//! best-effort; the renderer degrades to the metadata-only block when the log
//! can't be fetched.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config::LoadedConfig;
use crate::gh::{GhAuthPolicy, GhClient, GhSupervision};
use crate::identity::RuntimeMode;

/// Default tail size in bytes when scanning a job log.
pub const DEFAULT_LOG_TAIL_BYTES: usize = 262_144;

/// Allow-list of failure parsers tracked-repo config may select.
pub const ALLOWED_PARSERS: &[&str] = &["ctest", "catch2", "pytest", "go", "auto"];

/// Why diagnostics were not produced for a failed cloud target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DiagnosticsError {
    /// No `gh` auth, no repo slug, or other configuration problem.
    Config(String),
    /// `gh api` invocation failed.
    GhApi(String),
    /// API response could not be parsed.
    Parse(String),
    /// No failed job appears in the workflow-run jobs listing.
    NoFailedJob,
    /// Job log could not be retrieved.
    LogFetch(String),
}

impl fmt::Display for DiagnosticsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(message) => write!(f, "diagnostics config: {message}"),
            Self::GhApi(message) => write!(f, "diagnostics gh api: {message}"),
            Self::Parse(message) => write!(f, "diagnostics parse: {message}"),
            Self::NoFailedJob => write!(f, "diagnostics: no failed job in run"),
            Self::LogFetch(message) => write!(f, "diagnostics log: {message}"),
        }
    }
}

impl std::error::Error for DiagnosticsError {}

/// Metadata extracted for a single failing workflow job.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailedJobInfo {
    /// GitHub Actions job database ID.
    pub job_id: u64,
    /// Job display name (e.g. `macOS (ARM64) [github-hosted]`).
    pub name: String,
    /// HTML URL to the job inside the workflow run.
    pub html_url: String,
    /// Failing step name, if any.
    pub failed_step: Option<String>,
    /// Failing step exit-condition label (e.g. `failure`, `cancelled`).
    pub failed_step_conclusion: Option<String>,
    /// Runner labels (e.g. `namespace-profile-...`).
    pub runner_labels: Vec<String>,
    /// Runner display name when present.
    pub runner_name: Option<String>,
}

/// Final structured diagnostics that ride along on a failed cloud target.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct FailureDiagnostics {
    /// Logical Shipyard target name (e.g. `mac`).
    pub failed_target: String,
    /// Cloud workflow run ID.
    pub run_id: Option<u64>,
    /// Failing job metadata.
    pub job: Option<FailedJobInfo>,
    /// Up to N parsed failure summary lines (e.g. failing `CTest` IDs).
    pub failure_summary: Vec<String>,
    /// Whether the summary was truncated.
    pub failure_summary_truncated: bool,
    /// Raw last 256 KB tail of the failing job log (ANSI/group-stripped).
    /// Only retained for the JSON output; never included in the human render.
    /// `None` if the log could not be fetched.
    pub log_tail: Option<String>,
}

/// Failure-cause classification used by the renderer to pick a verb.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureKind {
    /// Tests / step failed.
    Failed,
    /// Workflow was cancelled (e.g. concurrency replaced).
    Cancelled,
    /// Hit Shipyard's poll deadline.
    TimedOut,
}

impl FailureKind {
    /// Map a GitHub Actions conclusion string to a `FailureKind`.
    #[must_use]
    pub fn from_conclusion(conclusion: Option<&str>) -> Self {
        match conclusion {
            Some("cancelled") => Self::Cancelled,
            Some("timed_out") => Self::TimedOut,
            _ => Self::Failed,
        }
    }
}

/// Parser trait: each implementation finds framework-specific failure
/// signatures in the last-N-KB of a job log.
pub trait FailureParser {
    /// Return failing-test lines parsed from `log_tail`.
    fn parse(&self, log_tail: &str) -> Vec<String>;
    /// Stable identifier used in logs / config-error messages.
    fn name(&self) -> &'static str;
}

/// `CTest` "The following tests FAILED:" block parser.
pub struct CtestParser;

impl FailureParser for CtestParser {
    fn name(&self) -> &'static str {
        "ctest"
    }

    fn parse(&self, log_tail: &str) -> Vec<String> {
        // GHA logs prefix every line with an ISO-8601 timestamp + space;
        // strip the prefix per-line so the "Errors while running CTest"
        // boundary and the leading whitespace on each failing-test row both
        // match cleanly.
        let payloads: Vec<&str> = log_tail.lines().map(strip_timestamp_prefix).collect();
        let Some(idx) = payloads
            .iter()
            .rposition(|line| line.contains("The following tests FAILED:"))
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for line in payloads.iter().skip(idx + 1) {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with("Errors while running CTest") {
                break;
            }
            // CTest puts a tab + "<id> - <name> (Failed)"; sometimes leading whitespace.
            let payload = trimmed.trim_start_matches([' ', '\t']).to_owned();
            if !payload.is_empty() {
                out.push(payload);
            }
        }
        out
    }
}

/// Catch2 `===` block parser. Each failed test ends with a `... | N failed` line
/// inside a banner of `=` separators. We capture the test-name line preceding
/// `[FAILED]`.
pub struct Catch2Parser;

impl FailureParser for Catch2Parser {
    fn name(&self) -> &'static str {
        "catch2"
    }

    fn parse(&self, log_tail: &str) -> Vec<String> {
        let mut out = Vec::new();
        // Strip GHA timestamp prefixes once so back-walk + `[FAILED]` matches
        // work on the raw test-framework payload.
        let lines: Vec<&str> = log_tail.lines().map(strip_timestamp_prefix).collect();
        for (idx, line) in lines.iter().enumerate() {
            if line.contains("[FAILED]") {
                // Walk back to the closest preceding `===` separator and grab
                // the next non-separator line as the test name.
                let mut name = None;
                for back in lines.iter().take(idx).rev() {
                    let trimmed = back.trim();
                    if trimmed.starts_with("====") {
                        continue;
                    }
                    if !trimmed.is_empty() {
                        name = Some(trimmed.to_owned());
                        break;
                    }
                }
                if let Some(name) = name {
                    out.push(name);
                }
            }
        }
        out
    }
}

/// Pytest summary parser — finds `FAILED <test>` lines.
pub struct PytestParser;

impl FailureParser for PytestParser {
    fn name(&self) -> &'static str {
        "pytest"
    }

    fn parse(&self, log_tail: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in log_tail.lines() {
            // Allow optional GHA timestamp prefix like `2026-05-18T22:40:51.7Z `.
            let candidate = strip_timestamp_prefix(line).trim_start();
            if let Some(rest) = candidate.strip_prefix("FAILED ") {
                out.push(format!("FAILED {}", rest.trim()));
            }
        }
        out
    }
}

/// Go test parser — `--- FAIL: TestName (...)` lines.
pub struct GoParser;

impl FailureParser for GoParser {
    fn name(&self) -> &'static str {
        "go"
    }

    fn parse(&self, log_tail: &str) -> Vec<String> {
        let mut out = Vec::new();
        for line in log_tail.lines() {
            let candidate = strip_timestamp_prefix(line).trim();
            if let Some(rest) = candidate.strip_prefix("--- FAIL: ") {
                out.push(format!("--- FAIL: {}", rest.trim()));
            }
        }
        out
    }
}

/// Try each parser in registry order; return the first that yields any hits.
pub struct AutoParser;

impl FailureParser for AutoParser {
    fn name(&self) -> &'static str {
        "auto"
    }

    fn parse(&self, log_tail: &str) -> Vec<String> {
        for parser in registry_excluding_auto() {
            let hits = parser.parse(log_tail);
            if !hits.is_empty() {
                return hits;
            }
        }
        Vec::new()
    }
}

fn registry_excluding_auto() -> Vec<Box<dyn FailureParser>> {
    vec![
        Box::new(CtestParser),
        Box::new(Catch2Parser),
        Box::new(PytestParser),
        Box::new(GoParser),
    ]
}

/// Validate a tracked-repo `failure_parser` config value against the allow-list.
///
/// Returns `Ok(name)` (canonical lower-cased) when accepted, `Err(_)` otherwise.
pub fn validate_parser_name(raw: &str) -> Result<String, DiagnosticsError> {
    let normalized = raw.trim().to_ascii_lowercase();
    if ALLOWED_PARSERS.contains(&normalized.as_str()) {
        Ok(normalized)
    } else {
        Err(DiagnosticsError::Config(format!(
            "unknown failure_parser '{raw}': allowed values are {}",
            ALLOWED_PARSERS.join(", ")
        )))
    }
}

/// Resolve a parser by name. Falls back to `auto` when `None`.
#[must_use]
pub fn select_parser(name: Option<&str>) -> Box<dyn FailureParser> {
    match name.map(str::to_ascii_lowercase).as_deref() {
        Some("ctest") => Box::new(CtestParser),
        Some("catch2") => Box::new(Catch2Parser),
        Some("pytest") => Box::new(PytestParser),
        Some("go") => Box::new(GoParser),
        _ => Box::new(AutoParser),
    }
}

// --- Log-tail utilities ---------------------------------------------------

/// Return the last `max_bytes` of `raw`, stripped of ANSI escapes and
/// `::group::` / `::endgroup::` markers. Length is on the raw bytes,
/// post-strip the string may be slightly shorter.
#[must_use]
pub fn log_tail_clean(raw: &str, max_bytes: usize) -> String {
    let bytes = raw.as_bytes();
    let start = bytes.len().saturating_sub(max_bytes);
    // Align to a UTF-8 char boundary.
    let mut start = start;
    while start < bytes.len() && (bytes[start] & 0b1100_0000) == 0b1000_0000 {
        start += 1;
    }
    let tail = &raw[start..];
    strip_ansi_and_groups(tail)
}

/// Strip ANSI CSI sequences and GitHub Actions group markers from `input`.
#[must_use]
pub fn strip_ansi_and_groups(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{001b}' {
            // ESC. Consume up to the final byte of a CSI sequence.
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                // Two-char ESC sequence (rare in CI logs); drop the next char.
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out.lines()
        .filter_map(|line| {
            let stripped = strip_timestamp_prefix(line);
            let trimmed = stripped.trim_start();
            if trimmed.starts_with("##[group]")
                || trimmed.starts_with("##[endgroup]")
                || trimmed.starts_with("::group::")
                || trimmed.starts_with("::endgroup::")
            {
                None
            } else {
                Some(line.to_owned())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// GHA log lines are usually prefixed with an ISO-8601 timestamp followed by
/// whitespace. Strip the prefix when present so parsers can match raw payload.
fn strip_timestamp_prefix(line: &str) -> &str {
    // Pattern: `2026-05-18T22:40:51.7294620Z `
    let bytes = line.as_bytes();
    if bytes.len() < 21 {
        return line;
    }
    if !bytes[0].is_ascii_digit() || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return line;
    }
    // Find first whitespace after position 19.
    for (i, &b) in bytes.iter().enumerate().skip(19) {
        if b == b' ' || b == b'\t' {
            return &line[i + 1..];
        }
    }
    line
}

// --- gh-backed fetcher ----------------------------------------------------

/// Side-effect boundary: fetch failing-job metadata + log tail.
///
/// Production impl shells out to `gh api`; tests use a fake.
pub trait DiagnosticsFetcher {
    /// Fetch the list of jobs for a workflow run as raw JSON.
    fn fetch_jobs_json(&self, repo: &str, run_id: u64) -> Result<String, DiagnosticsError>;
    /// Fetch the raw log for a job (returns the full log; caller tails it).
    fn fetch_job_log(&self, repo: &str, job_id: u64) -> Result<String, DiagnosticsError>;
}

/// `gh api`-backed [`DiagnosticsFetcher`].
#[derive(Clone, Debug)]
pub struct GhDiagnosticsFetcher {
    cwd: PathBuf,
    gh: Result<GhClient, String>,
}

impl GhDiagnosticsFetcher {
    /// Build a diagnostics fetcher that loads Shipyard config from `cwd`.
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let gh = GhClient::from_cwd(RuntimeMode::Shipyard, &cwd)
            .map_err(|error| format!("failed to load GitHub auth config for diagnostics: {error}"));
        Self { cwd, gh }
    }

    /// Build a diagnostics fetcher from an already loaded Shipyard config.
    #[must_use]
    pub fn from_loaded_config(cwd: impl Into<PathBuf>, config: &LoadedConfig) -> Self {
        let cwd = cwd.into();
        let gh = GhClient::from_loaded_config(config)
            .map_err(|error| format!("failed to load GitHub auth config for diagnostics: {error}"));
        Self { cwd, gh }
    }

    fn run_gh_api(&self, args: &[String]) -> Result<String, String> {
        let client = self.gh.as_ref().map_err(Clone::clone)?;
        let output = client
            .prepare_command(
                &self.cwd,
                None,
                GhSupervision::Unsupervised,
                GhAuthPolicy::Default,
            )
            .map_err(|error| format!("failed to prepare gh: {error}"))?
            .args(args)
            .output()
            .map_err(|error| format!("failed to spawn gh: {error}"))?;
        if !output.status.success() {
            let code = output
                .status
                .code()
                .map_or_else(|| "signal".to_owned(), |c| c.to_string());
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            return Err(format!("gh exited with status {code}: {stderr}"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

impl Default for GhDiagnosticsFetcher {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl DiagnosticsFetcher for GhDiagnosticsFetcher {
    fn fetch_jobs_json(&self, repo: &str, run_id: u64) -> Result<String, DiagnosticsError> {
        let path = format!("/repos/{repo}/actions/runs/{run_id}/jobs?per_page=100");
        let args = vec![
            "api".to_owned(),
            "-H".to_owned(),
            "Accept: application/vnd.github+json".to_owned(),
            path,
        ];
        self.run_gh_api(&args).map_err(DiagnosticsError::GhApi)
    }

    fn fetch_job_log(&self, repo: &str, job_id: u64) -> Result<String, DiagnosticsError> {
        let path = format!("/repos/{repo}/actions/jobs/{job_id}/logs");
        let args = vec!["api".to_owned(), path];
        self.run_gh_api(&args).map_err(DiagnosticsError::LogFetch)
    }
}

// --- end-to-end orchestrator ---------------------------------------------

/// Parse the response of `GET /repos/{repo}/actions/runs/{run_id}/jobs` into
/// the first failed [`FailedJobInfo`] (or `Err(NoFailedJob)`).
pub fn parse_failed_job(jobs_json: &str) -> Result<FailedJobInfo, DiagnosticsError> {
    let parsed: serde_json::Value = serde_json::from_str(jobs_json)
        .map_err(|error| DiagnosticsError::Parse(format!("jobs json: {error}")))?;
    // The endpoint returns `{ total_count, jobs: [...] }`; also accept a raw
    // single-job payload for the by-id endpoint, used in unit tests.
    let jobs = if let Some(array) = parsed.get("jobs").and_then(|value| value.as_array()) {
        array.clone()
    } else if parsed.is_object() {
        vec![parsed.clone()]
    } else {
        return Err(DiagnosticsError::Parse(
            "expected `jobs` array or object".to_owned(),
        ));
    };

    for job in &jobs {
        let conclusion = job.get("conclusion").and_then(serde_json::Value::as_str);
        if !matches!(conclusion, Some("failure" | "cancelled" | "timed_out")) {
            continue;
        }
        let id = job
            .get("id")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let name = job
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let html_url = job
            .get("html_url")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_owned();
        let mut failed_step = None;
        let mut failed_step_conclusion = None;
        if let Some(steps) = job.get("steps").and_then(|v| v.as_array()) {
            for step in steps {
                let step_conclusion = step.get("conclusion").and_then(|v| v.as_str());
                if matches!(step_conclusion, Some("failure" | "cancelled" | "timed_out")) {
                    failed_step = step
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(ToOwned::to_owned);
                    failed_step_conclusion = step_conclusion.map(ToOwned::to_owned);
                    break;
                }
            }
        }
        let runner_labels = job
            .get("labels")
            .and_then(|v| v.as_array())
            .map(|array| {
                array
                    .iter()
                    .filter_map(|v| v.as_str())
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let runner_name = job
            .get("runner_name")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        return Ok(FailedJobInfo {
            job_id: id,
            name,
            html_url,
            failed_step,
            failed_step_conclusion,
            runner_labels,
            runner_name,
        });
    }
    Err(DiagnosticsError::NoFailedJob)
}

/// Cap the number of summary lines we render inline.
pub const MAX_SUMMARY_LINES: usize = 8;

/// Resolve diagnostics for one cloud target end-to-end. Best-effort: missing
/// pieces degrade gracefully so the renderer can still emit *something*.
#[must_use]
pub fn fetch_failed_job_diagnostics<F: DiagnosticsFetcher + ?Sized>(
    fetcher: &F,
    repo: &str,
    run_id: u64,
    target_name: &str,
    parser: &dyn FailureParser,
) -> FailureDiagnostics {
    let mut out = FailureDiagnostics {
        failed_target: target_name.to_owned(),
        run_id: Some(run_id),
        ..FailureDiagnostics::default()
    };
    let jobs_json = match fetcher.fetch_jobs_json(repo, run_id) {
        Ok(text) => text,
        Err(error) => {
            out.failure_summary.push(format!("(diagnostics: {error})"));
            return out;
        }
    };
    let job = match parse_failed_job(&jobs_json) {
        Ok(info) => info,
        Err(error) => {
            out.failure_summary.push(format!("(diagnostics: {error})"));
            return out;
        }
    };
    let log_raw = fetcher.fetch_job_log(repo, job.job_id).ok();
    let log_clean = log_raw
        .as_deref()
        .map(|raw| log_tail_clean(raw, DEFAULT_LOG_TAIL_BYTES));
    if let Some(tail) = log_clean.as_deref() {
        let mut hits = parser.parse(tail);
        let truncated = hits.len() > MAX_SUMMARY_LINES;
        if truncated {
            hits.truncate(MAX_SUMMARY_LINES);
        }
        out.failure_summary = hits;
        out.failure_summary_truncated = truncated;
        out.log_tail = log_clean;
    } else if log_raw.is_none() {
        out.failure_summary
            .push("(diagnostics: job log unavailable)".to_owned());
    }
    // #344: a self-hosted runner returns an EMPTY GitHub job log, so the footer
    // parse above yields nothing and the failure block would be silent (only
    // "exit code N" reaches the user). The per-test results DO exist — a
    // well-behaved consuming workflow uploads them on failure (e.g. Pulp's
    // `ctest-logs-*` artifact + a job-summary block, pulp #3392/#3394). Point
    // there instead of emitting an empty summary. (A future enhancement can
    // download + parse that artifact for a structured list; tracked in #344.)
    if out.failure_summary.is_empty()
        && job.runner_labels.iter().any(|label| label == "self-hosted")
    {
        out.failure_summary.push(format!(
            "(self-hosted runner: GitHub job log is empty — open the run's job \
             summary, or download per-test logs with \
             `gh run download {run_id} -n ctest-logs-<key>`)"
        ));
    }
    out.job = Some(job);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL_CTEST_FOOTER: &str = include_str!("../tests/fixtures/ctest_failed_macos.log");
    const REAL_JOB_JSON: &str = include_str!("../tests/fixtures/job_76630095261.json");

    #[test]
    fn ctest_parser_extracts_failed_block() {
        let cleaned = log_tail_clean(REAL_CTEST_FOOTER, DEFAULT_LOG_TAIL_BYTES);
        let hits = CtestParser.parse(&cleaned);
        assert_eq!(
            hits,
            vec![
                "1236 - FontResolver: animation respects LRU cache cap (Failed)".to_owned(),
                "1237 - FontResolver: capacity=0 disables cap (legacy unbounded) (Failed)"
                    .to_owned(),
                "1238 - FontResolver: shrinking the cap evicts oldest immediately (Failed)"
                    .to_owned(),
                "1239 - FontResolver: LRU hit promotes entry past eviction line (Failed)"
                    .to_owned(),
            ],
            "expected the real CTest footer to yield 4 failing tests"
        );
    }

    #[test]
    fn ctest_parser_handles_synthetic_fixture() {
        let synthetic = "
prep noise
The following tests FAILED:
\t10 - my_test (Failed)
\t11 - other_test (Failed)
Errors while running CTest
";
        let hits = CtestParser.parse(synthetic);
        assert_eq!(
            hits,
            vec![
                "10 - my_test (Failed)".to_owned(),
                "11 - other_test (Failed)".to_owned(),
            ]
        );
    }

    #[test]
    fn catch2_parser_extracts_failed_block() {
        let log = "
===============================================================================
runtime smoke
===============================================================================
test_thing - assertion fired
[FAILED]
test_summary: 1 of 2 assertions failed
[FAILED]
";
        let hits = Catch2Parser.parse(log);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].contains("test_thing"));
    }

    #[test]
    fn pytest_parser_extracts_failed_lines() {
        let log = "
===== test session starts =====
test_foo.py::test_a PASSED
test_foo.py::test_b FAILED
====== short test summary info =======
FAILED test_foo.py::test_b - AssertionError
FAILED test_foo.py::test_c - RuntimeError: boom
== 2 failed, 1 passed in 0.5s ==
";
        let hits = PytestParser.parse(log);
        assert_eq!(hits.len(), 2);
        assert!(hits[0].contains("test_foo.py::test_b"));
        assert!(hits[1].contains("test_foo.py::test_c"));
    }

    #[test]
    fn go_parser_extracts_failed_lines() {
        let log = "
=== RUN   TestAdd
--- PASS: TestAdd (0.00s)
=== RUN   TestSub
    main_test.go:42: subtract is busted
--- FAIL: TestSub (0.00s)
FAIL    example/calc 0.013s
";
        let hits = GoParser.parse(log);
        assert_eq!(hits, vec!["--- FAIL: TestSub (0.00s)".to_owned()]);
    }

    #[test]
    fn auto_parser_falls_through_on_unknown_format() {
        let log = "nothing recognisable here\nno test framework\n";
        let hits = AutoParser.parse(log);
        assert!(hits.is_empty(), "auto parser must not invent hits");
    }

    #[test]
    fn auto_parser_picks_ctest_when_present() {
        let log = "
random noise
The following tests FAILED:
\t1 - sample (Failed)
";
        let hits = AutoParser.parse(log);
        assert_eq!(hits, vec!["1 - sample (Failed)".to_owned()]);
    }

    #[test]
    fn log_tail_strips_ansi_and_groups() {
        let raw =
            "##[group]Setup\nclean line\n\u{001b}[31mred bit\u{001b}[0m next\n::endgroup::\ndone";
        let cleaned = strip_ansi_and_groups(raw);
        assert!(!cleaned.contains('\u{001b}'));
        assert!(!cleaned.contains("##[group]"));
        assert!(!cleaned.contains("::endgroup::"));
        assert!(cleaned.contains("clean line"));
        assert!(cleaned.contains("red bit next"));
        assert!(cleaned.contains("done"));
    }

    #[test]
    fn log_tail_caps_at_256kb() {
        // 1 MB of `a`s with a sentinel near the end.
        let mut raw = "a".repeat(1_048_576 - 64);
        raw.push_str("SENTINEL_TAIL_MARKER\n");
        raw.push_str(&"b".repeat(40));
        let tailed = log_tail_clean(&raw, DEFAULT_LOG_TAIL_BYTES);
        assert!(
            tailed.len() <= DEFAULT_LOG_TAIL_BYTES,
            "tail length {} exceeded cap {}",
            tailed.len(),
            DEFAULT_LOG_TAIL_BYTES
        );
        assert!(tailed.contains("SENTINEL_TAIL_MARKER"));
        // We trimmed the front: assert the first 'a' is at the very start.
        assert_eq!(tailed.chars().next(), Some('a'));
    }

    #[test]
    fn select_parser_rejects_unknown_value() {
        let err = validate_parser_name("rogue").unwrap_err();
        match err {
            DiagnosticsError::Config(message) => {
                assert!(message.contains("rogue"));
            }
            other => panic!("expected Config error, got {other:?}"),
        }
    }

    #[test]
    fn select_parser_accepts_registry_values() {
        for name in ALLOWED_PARSERS {
            assert!(validate_parser_name(name).is_ok());
        }
    }

    #[test]
    fn parse_failed_job_extracts_macos_metadata_from_real_fixture() {
        let info = parse_failed_job(REAL_JOB_JSON).expect("real fixture must yield a failed job");
        assert_eq!(info.job_id, 76_630_095_261);
        assert_eq!(info.name, "macOS (ARM64) [github-hosted]");
        assert!(
            info.html_url
                .ends_with("/actions/runs/26063806409/job/76630095261")
        );
        assert_eq!(info.failed_step.as_deref(), Some("Test (non-Windows)"));
        assert_eq!(info.failed_step_conclusion.as_deref(), Some("failure"));
        assert!(
            info.runner_labels
                .iter()
                .any(|label| label == "namespace-profile-generouscorp-macos"),
            "expected to see namespace runner label, got {:?}",
            info.runner_labels
        );
    }

    #[test]
    fn parse_failed_job_wraps_runs_endpoint_envelope() {
        let payload = serde_json::json!({
            "total_count": 1,
            "jobs": [{
                "id": 5,
                "name": "mac",
                "html_url": "https://example/x",
                "conclusion": "failure",
                "steps": [{"name": "Build", "conclusion": "failure"}],
                "labels": ["macos-latest"]
            }]
        });
        let info = parse_failed_job(&payload.to_string()).unwrap();
        assert_eq!(info.job_id, 5);
        assert_eq!(info.failed_step.as_deref(), Some("Build"));
    }

    #[test]
    fn parse_failed_job_returns_no_failed_job_for_green_run() {
        let payload = serde_json::json!({
            "total_count": 1,
            "jobs": [{
                "id": 1, "name": "ok", "html_url": "u", "conclusion": "success", "steps": []
            }]
        });
        assert_eq!(
            parse_failed_job(&payload.to_string()),
            Err(DiagnosticsError::NoFailedJob)
        );
    }

    struct FakeFetcher {
        jobs_json: String,
        log: String,
    }

    impl DiagnosticsFetcher for FakeFetcher {
        fn fetch_jobs_json(&self, _repo: &str, _run_id: u64) -> Result<String, DiagnosticsError> {
            Ok(self.jobs_json.clone())
        }
        fn fetch_job_log(&self, _repo: &str, _job_id: u64) -> Result<String, DiagnosticsError> {
            Ok(self.log.clone())
        }
    }

    #[test]
    fn fetch_failed_job_diagnostics_end_to_end_with_real_fixture() {
        let fetcher = FakeFetcher {
            jobs_json: REAL_JOB_JSON.to_owned(),
            log: REAL_CTEST_FOOTER.to_owned(),
        };
        let diag = fetch_failed_job_diagnostics(
            &fetcher,
            "danielraffel/pulp",
            26_063_806_409,
            "mac",
            &*select_parser(Some("ctest")),
        );
        assert_eq!(diag.failed_target, "mac");
        assert_eq!(diag.run_id, Some(26_063_806_409));
        let job = diag.job.as_ref().expect("job metadata present");
        assert_eq!(job.failed_step.as_deref(), Some("Test (non-Windows)"));
        assert_eq!(diag.failure_summary.len(), 4);
        assert!(diag.failure_summary[0].starts_with("1236 -"));
        assert!(!diag.failure_summary_truncated);
    }

    #[test]
    fn fetch_failed_job_diagnostics_truncates_long_summary() {
        use std::fmt::Write as _;
        // Synthesize 12 failing tests so we trip the 8-line cap.
        let mut log = String::from("The following tests FAILED:\n");
        for i in 0..12 {
            writeln!(log, "\t{i} - test_{i} (Failed)").unwrap();
        }
        log.push_str("Errors while running CTest\n");
        let jobs = serde_json::json!({
            "jobs": [{
                "id": 1, "name": "mac",
                "html_url": "https://example/runs/9/job/1",
                "conclusion": "failure",
                "steps": [{"name": "Test", "conclusion": "failure"}],
                "labels": []
            }]
        });
        let fetcher = FakeFetcher {
            jobs_json: jobs.to_string(),
            log,
        };
        let diag = fetch_failed_job_diagnostics(
            &fetcher,
            "owner/repo",
            9,
            "mac",
            &*select_parser(Some("ctest")),
        );
        assert_eq!(diag.failure_summary.len(), MAX_SUMMARY_LINES);
        assert!(diag.failure_summary_truncated);
    }

    #[test]
    fn self_hosted_empty_log_points_to_ctest_artifact() {
        // #344: a self-hosted leg returns an empty GitHub job log, so the footer
        // parse yields nothing. Instead of a silent empty summary, point to the
        // uploaded per-test logs.
        let jobs = serde_json::json!({
            "jobs": [{
                "id": 1, "name": "macOS (ARM64) [local]",
                "html_url": "https://example/runs/9/job/1",
                "conclusion": "failure",
                "steps": [{"name": "Test (non-Windows)", "conclusion": "failure"}],
                "labels": ["self-hosted", "macOS", "ARM64"]
            }]
        });
        let fetcher = FakeFetcher {
            jobs_json: jobs.to_string(),
            log: String::new(), // self-hosted: GitHub job log is empty
        };
        let diag = fetch_failed_job_diagnostics(
            &fetcher,
            "danielraffel/pulp",
            9,
            "mac",
            &*select_parser(Some("ctest")),
        );
        assert_eq!(diag.failure_summary.len(), 1);
        assert!(
            diag.failure_summary[0].contains("self-hosted runner")
                && diag.failure_summary[0].contains("ctest-logs"),
            "expected a pointer to the ctest-logs artifact, got: {:?}",
            diag.failure_summary
        );
    }

    #[test]
    fn failure_kind_classifies_conclusions() {
        assert_eq!(
            FailureKind::from_conclusion(Some("cancelled")),
            FailureKind::Cancelled
        );
        assert_eq!(
            FailureKind::from_conclusion(Some("timed_out")),
            FailureKind::TimedOut
        );
        assert_eq!(
            FailureKind::from_conclusion(Some("failure")),
            FailureKind::Failed
        );
        assert_eq!(FailureKind::from_conclusion(None), FailureKind::Failed);
    }
}
