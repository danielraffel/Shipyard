//! C2: rules-only failing-line extraction, one rule per log format.
//!
//! Input is a GitHub Actions job log (timestamps allowed), a merge-queue
//! timeline event, or the error a job-log fetch returned; output is a list of
//! [`Record`]s in the `c2.cause.v0` shape: which rule fired, what kind of
//! failure, the most specific stable name for it, its first detail line, a
//! `file[:line]`, the 1-based line of the log where it starts, and whether
//! that line sits in a step that ended in `##[error]`. No network and no
//! model.
//!
//! This is the fleet's one cause extractor. Its contract is the fixture set in
//! `tests/fixtures/c2/` (one input per format with its expected records, plus
//! `regressions/`): a change to a rule lands with its fixture and a bump of
//! [`VERSION`]. It began as a port of EXP-A3's `c2_extract.py`, which stays
//! frozen at version 5 as the reference behind EXP-A3's registered results
//! and is not kept in step.
//!
//! Records from a step that ended in `##[error]` sort first, then by format
//! priority (the order of [`FORMATS`]), then by line, so the first record is
//! the cause: an assertion a retry already passed, or one a later step
//! recovered from, cannot outrank the runner dying.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::evidence::split_lines;

/// Record schema identifier.
pub const SCHEMA: &str = "c2.cause.v0";

/// The rule set's version, bumped with every rule change so a record says
/// which rules named it: 5 ranks make/ninja below the linker and keeps a lone
/// one unnamed; 6 adds `generated_drift`; 7 names a `CMake` error by its call
/// site and error rather than the bare command name, and a bare exit by the
/// last line it printed rather than the step's script line. EXP-A3 registers
/// each version before any re-score. Version 8 excludes explicit warning
/// diagnostics from linker causes, including the undefined-reference branch.
pub const VERSION: u32 = 8;

/// Every format, in priority order.
pub const FORMATS: [&str; 25] = [
    // A generated file that no longer matches its generator: the check names
    // the generator and the command that rewrites the file, so it outranks
    // the test that ran the check.
    "generated_drift",
    "ctest_summary",
    "pulp_affected",
    "ctest_inline",
    "unittest",
    "catch2",
    "cargo_test",
    "rust_panic",
    "rust_compile",
    "cargo_build",
    "node_test",
    "pytest",
    "python_traceback",
    "clang",
    "cmake",
    "linker",
    // A make or ninja failure names the target that broke, not why: it ranks
    // below every runner and compiler record that could explain it.
    "ninja",
    "gate_policy",
    "infra",
    "git_push",
    "github_api",
    "gha_annotation",
    "shell_exit",
    "merge_queue",
    "log_missing",
];

/// One cause record (`c2.cause.v0`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Which rule fired (one of [`FORMATS`]).
    pub format: String,
    /// What kind of failure (`test_failure`, `assertion`, `panic`,
    /// `compile_error`, `link_error`, `config_error`, `build_step`,
    /// `gate_policy`, `infra`, `push_refused`, `api_refused`, `annotation`,
    /// `exit_code`, `queue_ejection`, `log_missing`).
    pub kind: String,
    /// The most specific stable name: a test, an error code, a file, a step.
    pub subject: String,
    /// The first normalized detail line; empty when the format has none.
    pub message: String,
    /// Normalized `file[:line]` when the format gives one, else empty.
    pub location: String,
    /// 1-based line in the input where the record starts; 0 for a record
    /// that is not a log line.
    pub line: usize,
    /// Whether the line sits in a step that ended in `##[error]`; absent for
    /// a record that is not a log line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step_failed: Option<bool>,
}

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("c2 pattern compiles")
}

static TS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^\x{feff}?\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z ?"));
static ANSI: LazyLock<Regex> = LazyLock::new(|| pattern(r"\x1b\[[0-9;?]*[A-Za-z]"));
static RUNNER_ROOT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"(?:/Users/[^/\s]+/actions-runner/_work/[^/\s]+/[^/\s]+/",
        r"|/home/runner/work/[^/\s]+/[^/\s]+/",
        r"|[A-Z]:\\a\\[^\\\s]+\\[^\\\s]+\\",
        r"|/Volumes/[^/\s]+/[^\s]*?/_work/[^/\s]+/[^/\s]+/)"
    ))
});
static TMP: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?:/private)?/var/folders/[^\s]+?/T/[^\s/]+|/tmp/[^\s/]+"));
static HEX: LazyLock<Regex> = LazyLock::new(|| pattern(r"\b[0-9a-f]{12,64}\b"));
static ADDR: LazyLock<Regex> = LazyLock::new(|| pattern(r"\b0x[0-9a-fA-F]{6,}\b"));
static DURATION: LazyLock<Regex> = LazyLock::new(|| pattern(r"\b\d+(?:\.\d+)?\s?(?:sec|ms|s)\b"));
static SPACES: LazyLock<Regex> = LazyLock::new(|| pattern(r"\s+"));

/// The Actions timestamp prefix and ANSI colour removed, trailing space trimmed.
#[must_use]
pub fn strip_line(raw: &str) -> String {
    ANSI.replace_all(&TS.replace(raw, ""), "")
        .trim_end()
        .to_owned()
}

/// Volatile substrings normalized so the same failure on two runs matches.
#[must_use]
pub fn norm(text: &str, limit: usize) -> String {
    let text = RUNNER_ROOT.replace_all(text, "");
    let text = TMP.replace_all(&text, "<tmp>");
    let text = ADDR.replace_all(&text, "<addr>");
    let text = HEX.replace_all(&text, "<hex>");
    let text = DURATION.replace_all(&text, "<dur>");
    let text = SPACES.replace_all(&text, " ");
    text.trim().chars().take(limit).collect()
}

fn rec(
    format: &str,
    kind: &str,
    subject: &str,
    message: &str,
    location: &str,
    line: usize,
) -> Record {
    Record {
        format: format.to_owned(),
        kind: kind.to_owned(),
        subject: norm(subject, 160),
        message: norm(message, 200),
        location: norm(location, 160),
        line,
        step_failed: None,
    }
}

fn next_nonblank(lines: &[String], from: usize, limit: usize) -> Option<usize> {
    (from + 1..lines.len().min(from + 1 + limit)).find(|&j| !lines[j].trim().is_empty())
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

static CTEST_SUM_ROW: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^\s*\d+\s+-\s+(\S.*?)\s+\(([^)]+)\)"));

fn rule_ctest_summary(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "The following tests FAILED:" {
            continue;
        }
        for (j, row) in lines.iter().enumerate().skip(i + 1) {
            let Some(caps) = CTEST_SUM_ROW.captures(row) else {
                break;
            };
            out.push(rec(
                "ctest_summary",
                "test_failure",
                &caps[1],
                &caps[2],
                "",
                j + 1,
            ));
        }
    }
    out
}

/// Names may contain spaces (Catch2 `TEST_CASE` names); ctest pads them with dots.
static CTEST_INLINE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"Test\s+#\d+:\s+(.+?)\s+\.*\s*\*{3}(Failed|Exception|Timeout|Not Run|SEGFAULT|",
        r"Subprocess aborted|Child aborted|ILLEGAL|BUS|INTERRUPT|NUMERICAL)\b\s*(.*)$"
    ))
});
static CTEST_RESULT: LazyLock<Regex> = LazyLock::new(|| pattern(r"Test\s+#\d+:|^\s*Start\s+\d+:"));

/// Line ranges holding a failing ctest test's own output, or `None` when the
/// log has no ctest framing (a bare Catch2 run, where every block counts). A
/// block runs from the `***Failed` result line to the next result or `Start`
/// line.
fn failing_test_blocks(lines: &[String]) -> Option<Vec<(usize, usize)>> {
    if !lines.iter().any(|line| CTEST_RESULT.is_match(line)) {
        return None;
    }
    let mut blocks = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if CTEST_INLINE.is_match(line) {
            let end = (i + 1..lines.len())
                .find(|&j| CTEST_RESULT.is_match(&lines[j]))
                .unwrap_or(lines.len());
            blocks.push((i, end));
        }
    }
    Some(blocks)
}

fn rule_ctest_inline(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = CTEST_INLINE.captures(line)?;
            let message = format!("{} {}", &caps[2], &caps[3]);
            Some(rec(
                "ctest_inline",
                "test_failure",
                &caps[1],
                message.trim(),
                "",
                i + 1,
            ))
        })
        .collect()
}

static CATCH2_HEAD: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(\S+?\.(?:cpp|cc|cxx|hpp|h|mm)):(\d+):\s+FAILED:\s*$"));
static CATCH2_MACRO: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"^\s*((?:REQUIRE|CHECK|REQUIRE_FALSE|CHECK_FALSE|REQUIRE_THAT|CHECK_THAT|",
        r"REQUIRE_THROWS\w*|CHECK_THROWS\w*|REQUIRE_NOTHROW|CHECK_NOTHROW|FAIL)\s*\(.*)$"
    ))
});
static RULE_LINE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^-{20,}$"));
static ANY_RULE_LINE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^[-=~]{20,}$"));

/// The `TEST_CASE` (and section) name Catch2 prints between dashed rules
/// above a failure's location.
fn catch2_test(lines: &[String], i: usize) -> String {
    for j in (i.saturating_sub(11)..i).rev() {
        if !RULE_LINE.is_match(lines[j].trim()) {
            continue;
        }
        let mut first = j;
        while first > 0 {
            let above = lines[first - 1].trim();
            if above.is_empty() || ANY_RULE_LINE.is_match(above) {
                break;
            }
            first -= 1;
        }
        return lines[first..j]
            .iter()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
    }
    String::new()
}

fn rule_catch2(lines: &[String]) -> Vec<Record> {
    let blocks = failing_test_blocks(lines);
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = CATCH2_HEAD.captures(line.trim()) else {
            continue;
        };
        // An assertion outside every failing test's output block.
        if blocks
            .as_ref()
            .is_some_and(|blocks| !blocks.iter().any(|&(a, b)| a < i && i < b))
        {
            continue;
        }
        let location = basename(&caps[1]);
        let test = catch2_test(lines, i);
        let (mut expr, mut expansion) = (String::new(), String::new());
        if let Some(j) = next_nonblank(lines, i, 6) {
            expr = CATCH2_MACRO
                .captures(&lines[j])
                .map_or_else(|| lines[j].trim().to_owned(), |m| m[1].to_owned());
            if let Some(k) = next_nonblank(lines, j, 6)
                && lines[k].trim() == "with expansion:"
                && let Some(e) = next_nonblank(lines, k, 6)
            {
                lines[e].trim().clone_into(&mut expansion);
            }
        }
        let message = if expansion.is_empty() {
            expr
        } else {
            format!("{expr} => {expansion}")
        };
        let subject = if test.is_empty() { location } else { &test };
        out.push(rec(
            "catch2",
            "assertion",
            subject,
            &message,
            &format!("{location}:{}", &caps[2]),
            i + 1,
        ));
    }
    out
}

static PANIC: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"thread '([^']*)' panicked at ([^:\s]+):(\d+):\d+:?\s*$"));
static PANIC_OLD: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"thread '([^']*)' panicked at '(.*)', ([^:\s]+):(\d+):\d+"));

fn rule_rust_panic(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(caps) = PANIC.captures(line) {
            // Rust 1.73 and later print the message on the line after the location.
            let message = next_nonblank(lines, i, 2).map_or("", |j| lines[j].trim());
            let location = format!("{}:{}", basename(&caps[2]), &caps[3]);
            out.push(rec(
                "rust_panic",
                "panic",
                &caps[1],
                message,
                &location,
                i + 1,
            ));
        } else if let Some(caps) = PANIC_OLD.captures(line) {
            let location = format!("{}:{}", basename(&caps[3]), &caps[4]);
            out.push(rec(
                "rust_panic",
                "panic",
                &caps[1],
                &caps[2],
                &location,
                i + 1,
            ));
        }
    }
    out
}

static CARGO_PATH: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s{4}\S+$"));
static CARGO_NAME: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s{4}[A-Za-z_]\w*$"));

fn rule_cargo_test(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "failures:" {
            continue;
        }
        // cargo prints `failures:` twice: before the captured output blocks
        // (`---- name stdout ----`) and before the bare list of indented test
        // paths. Only the second is a list.
        let mut names = Vec::new();
        for (j, row) in lines.iter().enumerate().skip(i + 1) {
            if row.trim().is_empty() {
                if names.is_empty() {
                    continue;
                }
                break;
            }
            if (CARGO_PATH.is_match(row) && row.contains("::")) || CARGO_NAME.is_match(row) {
                names.push((j, row.trim()));
            } else {
                break;
            }
        }
        for (j, name) in names {
            out.push(rec("cargo_test", "test_failure", name, "", "", j + 1));
        }
    }
    out
}

static RUST_ERR: LazyLock<Regex> = LazyLock::new(|| pattern(r"^error\[(E\d{4})\]:\s*(.*)$"));
static RUST_ARROW: LazyLock<Regex> = LazyLock::new(|| pattern(r"^\s*-->\s*(\S+?):(\d+):\d+"));

fn rule_rust_compile(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = RUST_ERR.captures(line.trim()) else {
            continue;
        };
        let location = lines
            .iter()
            .take(i + 4)
            .skip(i + 1)
            .find_map(|row| RUST_ARROW.captures(row))
            .map_or_else(String::new, |m| format!("{}:{}", basename(&m[1]), &m[2]));
        out.push(rec(
            "rust_compile",
            "compile_error",
            &caps[1],
            &caps[2],
            &location,
            i + 1,
        ));
    }
    out
}

static CARGO_BUILD: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^error: could not compile `([^`]+)`(.*)$"));
static PARENTHESIS: LazyLock<Regex> = LazyLock::new(|| pattern(r"\(([^)]+)\)"));

fn rule_cargo_build(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = CARGO_BUILD.captures(line.trim())?;
            let unit = PARENTHESIS
                .captures(&caps[2])
                .map_or_else(String::new, |m| m[1].to_owned());
            Some(rec(
                "cargo_build",
                "compile_error",
                &caps[1],
                &unit,
                "",
                i + 1,
            ))
        })
        .collect()
}

static NODE_X: LazyLock<Regex> = LazyLock::new(|| pattern(r"^✖ (.+?) \(\d+(?:\.\d+)?ms\)$"));
static TAP: LazyLock<Regex> = LazyLock::new(|| pattern(r"^not ok \d+ - (.+?)(?:\s+#.*)?$"));
static NODE_DETAIL: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(?:error|message):"));
static NODE_ERROR: LazyLock<Regex> = LazyLock::new(|| pattern(r"^[A-Z]\w*Error\b"));

fn rule_node_test(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        let Some(caps) = NODE_X.captures(text).or_else(|| TAP.captures(text)) else {
            continue;
        };
        let mut message = String::new();
        for row in lines.iter().take(i + 12).skip(i + 1) {
            let row = row.trim();
            if NODE_DETAIL.is_match(row) || NODE_ERROR.is_match(row) {
                message = if row.starts_with("error:") || row.starts_with("message:") {
                    row.split_once(':')
                        .map_or("", |(_, rest)| rest)
                        .trim_matches([' ', '\'', '"'])
                        .to_owned()
                } else {
                    row.to_owned()
                };
                break;
            }
        }
        out.push(rec(
            "node_test",
            "test_failure",
            &caps[1],
            &message,
            "",
            i + 1,
        ));
    }
    out
}

static PYTEST: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(FAILED|ERROR) (\S+?\.py)(::\S+)?(?: - (.*))?$"));

fn rule_pytest(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = PYTEST.captures(line.trim())?;
            let path = basename(&caps[2]);
            let test = format!("{path}{}", caps.get(3).map_or("", |m| m.as_str()));
            let message = caps.get(4).map_or(&caps[1], |m| m.as_str());
            Some(rec("pytest", "test_failure", &test, message, path, i + 1))
        })
        .collect()
}

static UNITTEST: LazyLock<Regex> = LazyLock::new(|| pattern(r"^(FAIL|ERROR): (\w+) \(([\w.]+)\)"));
static UNITTEST_SEPARATOR: LazyLock<Regex> = LazyLock::new(|| pattern(r"^-{20,}$|^={20,}$"));
static EXCEPTION_LINE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[A-Z]\w*(Error|Exception|Exit)\b:?"));

fn rule_unittest(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = UNITTEST.captures(line.trim()) else {
            continue;
        };
        let mut message = String::new();
        // The assertion follows the traceback; stop at the next separator.
        for (j, row) in lines.iter().enumerate().take(i + 60).skip(i + 1) {
            let row = row.trim();
            if UNITTEST_SEPARATOR.is_match(row) && j > i + 1 {
                break;
            }
            if EXCEPTION_LINE.is_match(row) {
                row.clone_into(&mut message);
            }
        }
        let message = if message.is_empty() {
            caps[1].to_owned()
        } else {
            message
        };
        out.push(rec(
            "unittest",
            "test_failure",
            &caps[3],
            &message,
            "",
            i + 1,
        ));
    }
    out
}

static PY_FILE: LazyLock<Regex> = LazyLock::new(|| pattern(r#"^\s*File "([^"]+)", line (\d+)"#));
static PY_EXCEPTION: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^[A-Za-z_][\w.]*(Error|Exception|Exit|Interrupt|Warning)\b"));

fn rule_python_traceback(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if line.trim() != "Traceback (most recent call last):" {
            continue;
        }
        let mut location = String::new();
        let mut found: Option<(usize, String)> = None;
        for (j, row) in lines.iter().enumerate().take(i + 80).skip(i + 1) {
            if let Some(caps) = PY_FILE.captures(row) {
                location = format!("{}:{}", basename(&caps[1]), &caps[2]);
                continue;
            }
            if row.starts_with([' ', '\t']) || row.trim().is_empty() {
                continue;
            }
            if PY_EXCEPTION.is_match(row.trim()) {
                found = Some((j, row.trim().to_owned()));
            }
            break;
        }
        if let Some((end, message)) = found {
            let exception = message.split(':').next().unwrap_or(&message);
            let kind = if exception.ends_with("AssertionError") {
                "assertion"
            } else {
                "panic"
            };
            out.push(rec(
                "python_traceback",
                kind,
                exception,
                &message,
                &location,
                end + 1,
            ));
        }
    }
    out
}

static CMAKE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^CMake Error(?: at ([^\s:]+):(\d+))?(?: \(([^)]*)\))?:?\s*(.*)$"));

/// An error message up to its first sentence or clause end.
fn first_clause(message: &str) -> &str {
    let end = message
        .find(". ")
        .or_else(|| message.find("; "))
        .unwrap_or(message.len());
    message[..end].trim().trim_end_matches('.')
}

fn rule_cmake(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(caps) = CMAKE.captures(line.trim()) else {
            continue;
        };
        let mut message = caps[4].to_owned();
        if message.is_empty() {
            message =
                next_nonblank(lines, i, 3).map_or_else(String::new, |j| lines[j].trim().to_owned());
        }
        let location = caps.get(1).map_or_else(String::new, |path| {
            format!("{}:{}", basename(path.as_str()), &caps[2])
        });
        // The command in parentheses (`string`, `message`) is the CMake
        // command that failed, the same for every error it raises: the call
        // site and the error's first clause say which failure this is.
        let subject = match (location.is_empty(), caps.get(3)) {
            (false, _) => format!("{location} {}", first_clause(&message)),
            (true, Some(command)) => format!("{}: {}", command.as_str(), first_clause(&message)),
            (true, None) => "cmake".to_owned(),
        };
        out.push(rec(
            "cmake",
            "config_error",
            &subject,
            &message,
            &location,
            i + 1,
        ));
    }
    out
}

static CLANG: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"^(\S+?\.(?:c|cc|cpp|cxx|h|hpp|hh|m|mm|swift|inl|ipp)):(\d+):(?:\d+:)?\s+(?:fatal )?error:\s*(.*)$",
    )
});

fn rule_clang(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = CLANG.captures(line.trim())?;
            let file = basename(&caps[1]);
            Some(rec(
                "clang",
                "compile_error",
                file,
                &caps[3],
                &format!("{file}:{}", &caps[2]),
                i + 1,
            ))
        })
        .collect()
}

static LD_SYMBOL: LazyLock<Regex> = LazyLock::new(|| pattern(r#"^\s*"([^"]+)""#));
static UNDEFINED_REF: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"undefined reference to [`'](.+?)'"));
static LD_LINE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:ld|ld64\.lld|lld|/usr/bin/ld)(?:\.\w+)?: (?:error: )?(.*)$"));

static LINKER_SEVERITY: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?:^|:\s+)(warning|error|fatal error):(?:\s|$)"));

fn rule_linker(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        // A later cancellation can mark the whole step failed; that does not
        // make its linker warnings fatal. Check severity before the unanchored
        // undefined-reference match too. Use the first severity so an error
        // quoting "warning:" remains fatal; separate -fatal_warnings errors
        // and diagnostics promoted to "error:" still reach the rules below.
        if LINKER_SEVERITY
            .captures(text)
            .is_some_and(|caps| &caps[1] == "warning")
        {
            continue;
        }
        if text.starts_with("Undefined symbols for architecture") {
            let symbol = next_nonblank(lines, i, 2).map_or_else(String::new, |j| {
                LD_SYMBOL
                    .captures(&lines[j])
                    .map_or_else(|| lines[j].trim().to_owned(), |m| m[1].to_owned())
            });
            let subject = if symbol.is_empty() {
                "undefined symbols"
            } else {
                &symbol
            };
            out.push(rec("linker", "link_error", subject, text, "", i + 1));
            continue;
        }
        if let Some(caps) = UNDEFINED_REF.captures(text) {
            out.push(rec("linker", "link_error", &caps[1], text, "", i + 1));
            continue;
        }
        if let Some(caps) = LD_LINE.captures(text)
            && (text.contains("not found")
                || text.contains("error")
                || text.contains("duplicate symbol"))
        {
            out.push(rec("linker", "link_error", "ld", &caps[1], "", i + 1));
        }
    }
    out
}

static NINJA: LazyLock<Regex> = LazyLock::new(|| pattern(r"^FAILED: (?:\[code=\d+\] )?(\S+)"));
static MAKE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^make(?:\[\d+\])?: \*\*\* \[([^\]]+)\] Error (\d+)"));

static DRIFT_COUNT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^([A-Za-z0-9_.-]+): \d+ drift problems?(?:\(s\))?\b(.*)$"));
static DRIFT_STALE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^([A-Za-z0-9_.-]+): the .+ no longer matches .+$"));
static REGENERATE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"((?:python3?|pulp|bash|cmake|node)\s+\S+[^`]*?--write)\b"));
static REGENERATE_WITH: LazyLock<Regex> = LazyLock::new(|| pattern(r"(regenerate with[^:;.]*)"));

/// A drift check: `<generator>: N drift problem(s) ...` or `<generator>: the
/// <file> no longer matches <source>`. The subject is the generator; the
/// message is the command that rewrites the file, from the lines the check
/// prints after its header, else the header's own "regenerate with" phrase.
fn rule_generated_drift(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        // A bare "error: the ... no longer matches" is not a generator: a
        // generator's name carries `_` or `-`.
        let Some(caps) = DRIFT_COUNT.captures(text).or_else(|| {
            DRIFT_STALE
                .captures(text)
                .filter(|caps| caps[1].contains(['_', '-']))
        }) else {
            continue;
        };
        let generator = &caps[1];
        let command = lines
            .iter()
            .skip(i + 1)
            .take(60)
            .find_map(|next| REGENERATE.captures(next).map(|found| found[1].to_owned()))
            .or_else(|| {
                REGENERATE_WITH
                    .captures(text)
                    .map(|found| found[1].to_owned())
            })
            .unwrap_or_default();
        out.push(rec(
            "generated_drift",
            "generated_file_drift",
            generator,
            &command,
            "",
            i + 1,
        ));
    }
    out
}

fn rule_ninja(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        // Catch2 (`file:line: FAILED:`) and pytest (`FAILED x.py::t`) cannot
        // match: the first does not start with FAILED, the second has no colon.
        if let Some(caps) = NINJA.captures(text) {
            out.push(rec(
                "ninja",
                "build_step",
                basename(&caps[1]),
                "",
                "",
                i + 1,
            ));
        }
        if let Some(caps) = MAKE.captures(text) {
            let message = format!("Error {}", &caps[2]);
            out.push(rec(
                "ninja",
                "build_step",
                basename(&caps[1]),
                &message,
                "",
                i + 1,
            ));
        }
    }
    out
}

static AFFECTED: LazyLock<Regex> = LazyLock::new(|| {
    pattern(r"^##\[error\](\S+) failed on this pull request head\b[;,.]?\s*(.*)$")
});

fn rule_pulp_affected(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = AFFECTED.captures(line.trim())?;
            Some(rec(
                "pulp_affected",
                "test_failure",
                &caps[1],
                &caps[2],
                "",
                i + 1,
            ))
        })
        .collect()
}

/// A policy gate's verdict: (pattern, subject, message).
type GateRule = (
    Regex,
    fn(&regex::Captures<'_>) -> String,
    fn(&regex::Captures<'_>) -> String,
);

static GATE: LazyLock<[GateRule; 3]> = LazyLock::new(|| {
    [
        // The verdict's own trailing period is not a detail.
        (
            pattern(r"^(Skill-sync) check FAILED[.:]?\s*(.*)$"),
            |m| m[1].to_owned(),
            |m| m[2].to_owned(),
        ),
        // Pulp prints `✗ doc: ...`, Shipyard `✗ docs/<file>.md: ...`.
        (
            pattern(r"^✗ (\S+?): (mapped paths changed.*)$"),
            |m| {
                if &m[1] == "doc" {
                    "config-doc".to_owned()
                } else {
                    m[1].to_owned()
                }
            },
            |m| m[2].to_owned(),
        ),
        (
            pattern(
                r"^(?:##\[error\])?(.+?) did not succeed — failing (\w+) gate closed\b\s*(.*)$",
            ),
            |m| m[1].to_owned(),
            |m| format!("failing {} gate closed {}", &m[2], &m[3]),
        ),
    ]
});

/// Echoed script text naming a marker is not the event.
fn echoed(text: &str) -> bool {
    (text.starts_with("echo ") || text.starts_with("grep ") || text.starts_with('#'))
        && !text.starts_with("##[error]")
}

fn rule_gate_policy(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        if echoed(text) {
            continue;
        }
        for (rule, subject, message) in GATE.iter() {
            if let Some(caps) = rule.captures(text) {
                out.push(rec(
                    "gate_policy",
                    "gate_policy",
                    &subject(&caps),
                    &message(&caps),
                    "",
                    i + 1,
                ));
                break;
            }
        }
    }
    out
}

/// GitHub's or the system's own words for a runner, network or disk failure:
/// (pattern, class, subject; `None` takes the matched error code).
static INFRA: LazyLock<[(Regex, &'static str, Option<&'static str>); 7]> = LazyLock::new(|| {
    [
        (
            pattern(r"^##\[error\]The operation was canceled\.?$"),
            "cancelled",
            Some("operation canceled"),
        ),
        (
            pattern(r"Upload progress stalled"),
            "infra",
            Some("upload stalled"),
        ),
        (
            pattern(r"Exec format error"),
            "infra",
            Some("exec format error"),
        ),
        (
            pattern(r"\b(ENOTFOUND|ETIMEDOUT|ECONNRESET|ECONNREFUSED|EAI_AGAIN)\b"),
            "infra",
            None,
        ),
        (
            pattern(r"The runner has received a shutdown signal"),
            "infra",
            Some("runner shutdown signal"),
        ),
        (
            pattern(r"lost communication with the server"),
            "infra",
            Some("runner lost communication"),
        ),
        (
            pattern(r"No space left on device"),
            "infra",
            Some("disk full"),
        ),
    ]
});

/// The failure class of an `infra` record, by its subject.
#[must_use]
pub fn infra_class(subject: &str) -> &'static str {
    INFRA
        .iter()
        .find(|(_, _, name)| *name == Some(subject))
        .map_or("infra", |(_, class, _)| *class)
}

fn rule_infra(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        if echoed(text) {
            continue;
        }
        for (rule, _, subject) in INFRA.iter() {
            if let Some(caps) = rule.captures(text) {
                let subject = subject.map_or_else(|| caps[1].to_owned(), str::to_owned);
                let message = text.strip_prefix("##[error]").unwrap_or(text);
                out.push(rec("infra", "infra", &subject, message, "", i + 1));
                break;
            }
        }
    }
    out
}

static GH_REMOTE: LazyLock<Regex> = LazyLock::new(|| pattern(r"^remote: error: (GH\d{3}): (.*)$"));
static PUSH_REJECTED: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^! \[remote rejected\] (\S+) -> (\S+) \((.*)\)$"));

fn rule_git_push(lines: &[String]) -> Vec<Record> {
    let mut out = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let text = line.trim();
        if let Some(caps) = GH_REMOTE.captures(text) {
            out.push(rec(
                "git_push",
                "push_refused",
                &caps[1],
                &caps[2],
                "",
                i + 1,
            ));
        } else if let Some(caps) = PUSH_REJECTED.captures(text) {
            out.push(rec(
                "git_push",
                "push_refused",
                "remote rejected",
                &caps[3],
                &caps[2],
                i + 1,
            ));
        }
    }
    out
}

static API_405: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?:GitHub API|HTTP)\b.*?\b(405)\b|\b(405) Method Not Allowed\b"));

fn rule_github_api(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let text = line.trim();
            let caps = API_405.captures(text)?;
            let code = caps.get(1).or_else(|| caps.get(2))?.as_str();
            let message = text.strip_prefix("##[error]").unwrap_or(text);
            Some(rec(
                "github_api",
                "api_refused",
                &format!("http {code}"),
                message,
                "",
                i + 1,
            ))
        })
        .collect()
}

static EXIT: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^##\[error\]Process completed with exit code (\d+)\.?$"));
static GROUP_RUN: LazyLock<Regex> = LazyLock::new(|| pattern(r"^##\[group\]Run (.*)$"));
static RULER: LazyLock<Regex> = LazyLock::new(|| pattern(r"^[-=~*#]{3,}$"));
static SETTING: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:shell|env):|^[A-Z_][A-Z0-9_]+:\s"));

fn current_step(lines: &[String], i: usize) -> String {
    lines[..i]
        .iter()
        .rev()
        .find_map(|line| GROUP_RUN.captures(line))
        .map_or_else(String::new, |caps| caps[1].trim().to_owned())
}

/// The last meaningful output line before an exit annotation, read back
/// only to the step's own `##[endgroup]`: the lines before it are the step's
/// script, echoed by the runner, and name nothing.
fn tail(lines: &[String], i: usize) -> String {
    for j in (i.saturating_sub(39)..i).rev() {
        let text = lines[j].trim();
        if text.starts_with("##[endgroup]") || text.starts_with("##[group]") {
            break;
        }
        if text.is_empty() || text.starts_with("##[") || RULER.is_match(text) {
            continue;
        }
        if SETTING.is_match(text) {
            continue;
        }
        return text.to_owned();
    }
    String::new()
}

fn rule_shell_exit(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let caps = EXIT.captures(line.trim())?;
            let last = tail(lines, i);
            let mut message = format!("exit code {}", &caps[1]);
            if !last.is_empty() {
                message.push_str(": ");
                message.push_str(&last);
            }
            // The step's script line is the same for every way the step can
            // fail; the last line it printed is the failure.
            let subject = if last.is_empty() {
                current_step(lines, i)
            } else {
                last.clone()
            };
            Some(rec(
                "shell_exit",
                "exit_code",
                &subject,
                &message,
                "",
                i + 1,
            ))
        })
        .collect()
}

/// Whether a more specific rule already turns this `##[error]` line into a record.
fn owned(text: &str) -> bool {
    AFFECTED.is_match(text)
        || GATE.iter().any(|(rule, _, _)| rule.is_match(text))
        || INFRA.iter().any(|(rule, _, _)| rule.is_match(text))
        || API_405.is_match(text)
}

fn rule_gha_annotation(lines: &[String]) -> Vec<Record> {
    lines
        .iter()
        .enumerate()
        .filter_map(|(i, line)| {
            let text = line.trim();
            let message = text.strip_prefix("##[error]")?;
            (!EXIT.is_match(text) && !owned(text))
                .then(|| rec("gha_annotation", "annotation", "error", message, "", i + 1))
        })
        .collect()
}

type Rule = fn(&[String]) -> Vec<Record>;

/// The rules, in the order their records are gathered (ties in the sort
/// keep it).
const RULES: [Rule; 23] = [
    rule_generated_drift,
    rule_ctest_summary,
    rule_pulp_affected,
    rule_ctest_inline,
    rule_cargo_build,
    rule_node_test,
    rule_gate_policy,
    rule_infra,
    rule_git_push,
    rule_github_api,
    rule_catch2,
    rule_rust_panic,
    rule_cargo_test,
    rule_rust_compile,
    rule_pytest,
    rule_unittest,
    rule_python_traceback,
    rule_cmake,
    rule_clang,
    rule_linker,
    rule_ninja,
    rule_shell_exit,
    rule_gha_annotation,
];

/// Position of `format` in [`FORMATS`].
#[must_use]
pub fn priority(format: &str) -> usize {
    FORMATS
        .iter()
        .position(|known| *known == format)
        .unwrap_or(FORMATS.len())
}

static STEP_START: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:##\[group\]Run |##\[group\]Post |Post job cleanup\.)"));

/// Per line: does it sit in a step that ended with a `##[error]`? Steps are
/// delimited by `##[group]Run`, `##[group]Post` and `Post job cleanup.`. A
/// log with no `##[error]` at all (an excerpt) counts every line as failing.
fn failed_step_lines(lines: &[String]) -> Vec<bool> {
    let mut step = 0;
    let steps: Vec<usize> = lines
        .iter()
        .map(|line| {
            if STEP_START.is_match(line) {
                step += 1;
            }
            step
        })
        .collect();
    let bad: std::collections::BTreeSet<usize> = lines
        .iter()
        .zip(&steps)
        .filter(|(line, _)| line.starts_with("##[error]"))
        .map(|(_, step)| *step)
        .collect();
    if bad.is_empty() {
        return vec![true; lines.len()];
    }
    steps.iter().map(|step| bad.contains(step)).collect()
}

/// The log's lines as the rules read them: split like Python's
/// `str.splitlines`, each without its timestamp or colour.
#[must_use]
pub fn lines_of(text: &str) -> Vec<String> {
    split_lines(text).into_iter().map(strip_line).collect()
}

/// Every record in one log: failing-step records first, then by format
/// priority, then by line.
#[must_use]
pub fn extract(text: &str) -> Vec<Record> {
    extract_lines(&lines_of(text))
}

/// [`extract`] over already-split lines.
#[must_use]
pub fn extract_lines(lines: &[String]) -> Vec<Record> {
    let failed = failed_step_lines(lines);
    let mut records: Vec<Record> = RULES.iter().flat_map(|rule| rule(lines)).collect();
    for record in &mut records {
        record.step_failed = Some(record.line == 0 || failed[record.line - 1]);
    }
    records.sort_by_key(|record| {
        (
            record.step_failed != Some(true),
            priority(&record.format),
            record.line,
        )
    });
    records
}

/// The record of a merge-queue removal event from the issues timeline API.
#[must_use]
pub fn extract_merge_queue_event(event: &Value) -> Vec<Record> {
    if event.get("event").and_then(Value::as_str) != Some("removed_from_merge_queue") {
        return Vec::new();
    }
    let reason = ["reason", "removal_reason"]
        .iter()
        .find_map(|key| {
            event
                .get(*key)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
        .unwrap_or("");
    vec![rec(
        "merge_queue",
        "queue_ejection",
        "removed_from_merge_queue",
        reason,
        "",
        0,
    )]
}

static FETCH_ERROR: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"\b(BlobNotFound|HTTP 404|HTTP 410|Not Found|Gone)\b"));

/// The record of a job log that could not be fetched, from the logs
/// endpoint's error body.
#[must_use]
pub fn extract_log_fetch_error(error: &str) -> Vec<Record> {
    FETCH_ERROR
        .captures(error)
        .map(|caps| {
            vec![rec(
                "log_missing",
                "log_missing",
                "job log unavailable",
                &caps[1],
                "",
                0,
            )]
        })
        .unwrap_or_default()
}

/// Retrieval text: distinct records in order, at most twelve and 1500
/// characters.
#[must_use]
pub fn signature(records: &[Record]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    let mut parts = Vec::new();
    for record in records {
        if !seen.insert((&record.kind, &record.subject, &record.message)) {
            continue;
        }
        parts.push(
            format!("{} {}: {}", record.kind, record.subject, record.message)
                .trim()
                .to_owned(),
        );
        if parts.len() >= 12 {
            break;
        }
    }
    parts.join("\n").chars().take(1500).collect()
}

#[cfg(test)]
mod tests;
