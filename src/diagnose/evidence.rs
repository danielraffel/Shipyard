//! Evidence: which lines of a job log name the failure.
//!
//! Generic, with no repository knowledge:
//!
//! 1. Only the failing step's lines count, found from the step's start and end
//!    timestamps in the timestamped log. That alone removes checkout noise such
//!    as thousands of `[new branch]` fetch lines.
//! 2. Each test in the last `CTest` "The following tests FAILED" block gets a
//!    group from the output of its last failing attempt: whole when short (a
//!    guard's one-line verdict has no error keyword), else its highest-scoring
//!    lines. The summary block itself is never spent as evidence.
//! 3. Scored error lines elsewhere in the step form signal groups with two
//!    lines of context; once tests name the failure only hard errors qualify.
//! 4. When no test names the failure, the step's last lines before `Process
//!    completed with exit code` (the tail) carry the verdict of a lint, guard
//!    or configure error.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::LazyLock;

use regex::Regex;

use super::Step;

/// Most lines in one evidence group.
pub const MAX_GROUP_LINES: usize = 12;
/// Longest evidence line, in characters.
pub const MAX_LINE_CHARS: usize = 240;
const CONTEXT: usize = 2;
const TAIL_LINES: usize = 8;
const TEST_BLOCK_SCAN: usize = 5000;
const FAILED_BLOCK: &str = "The following tests FAILED";

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("diagnose evidence pattern compiles")
}

static TIMESTAMP: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z ?"));
static ANSI: LazyLock<Regex> = LazyLock::new(|| pattern(r"\x1b\[[0-9;?]*[A-Za-z]"));
/// Lines that never carry a cause: passing tests, ctest bookkeeping, fetch
/// output, runner command echo, group markers.
static NOISE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(concat!(
        r"^\s*(?:\d+/\d+ Test\s+#\d+: .*\bPassed\b|Start\s+\d+: |\S.*= +[\d.]+ sec\*proc|\d+ - .* \((?:Skipped|Disabled)\)|",
        r"\*\s+\[new (?:branch|tag|ref)\]|[+ ]\s*[0-9a-f]{7,}\.\.\.?[0-9a-f]{7,}\s|From https?://|",
        r"\[command\]|##\[(?:end)?group\]|::(?:end)?group::|##\[end-action |shell: |env:$)"
    ))
});
static EXIT_CODE: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^##\[error\]Process completed with exit code \d+\.?$"));
static FAILED_ENTRY: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"^\s*(\d+)\s+-\s+(.+?)\s+\((Failed|Timeout|Subprocess aborted|Exception|Not Run|SEGFAULT|ILLEGAL|BAD_COMMAND|OTHER_FAULT)[^)]*\)",
    )
});
static STATUS: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"Test\s+#(\d+):\s.*?(\*\*\*\w+|Passed|Not Run)"));
static ANY_STATUS: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"^\s*(?:(?:\d+/\d+\s+)?Test\s+#\d+:\s|Start\s+\d+:\s|\d+% tests passed|The following tests)",
    )
});
static RETRY_NAME_TAIL: LazyLock<Regex> = LazyLock::new(|| pattern(r"\s*\.{3,}.*$"));
static OTHER_FAILURES: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        pattern(r"^test (\S+) \.\.\. FAILED$"),
        pattern(r"^FAILED (\S+)"),
        pattern(r"^--- FAIL: (\S+)"),
        pattern(r"^(?:ERROR|FAIL): (\S+) \(([\w.]+)\)"),
    ]
});
static DIGITS: LazyLock<Regex> = LazyLock::new(|| pattern(r"\d+"));
/// Line scores; the first pattern that matches decides.
static SCORES: LazyLock<Vec<(i32, Regex)>> = LazyLock::new(|| {
    vec![
        (0, pattern(r"(?i)^\s*warning\b|\bwarning:")),
        (0, pattern(r"Process completed with exit code")),
        (
            10,
            pattern(concat!(
                r"panicked at|\bFAILED:|^FAIL:|^\s*(?:REQUIRE|CHECK)\w*\(|Assertion(?:Error)? |assert(?:ion)? failed|",
                r"CMake Error|^Traceback|\b\w+Error: |\berror(?:\[E\d+\])?: |^Error: |\bfatal: |##\[error\]|",
                r"✗|HTTP Error \d{3}|FAILED\b|\bFAIL\b"
            )),
        ),
        (
            6,
            pattern(concat!(
                r"(?i)\bwith (?:expansion|message):|\bexpected\b.*\bgot\b|\bnot found\b|\bdenied\b|\btimed out\b|",
                r"\bno such\b"
            )),
        ),
        (3, pattern(r"(?i)\bfail(?:ed|ure|s)?\b|\berror\b|\bviolat")),
    ]
});

/// Split like Python's `str.splitlines`: every Unicode line boundary, `\r\n`
/// as one, no empty element for a trailing boundary. Logs carry bare `\r`
/// (progress output), and the line numbers the diagnosis reports depend on
/// splitting it the same way every time.
#[must_use]
pub fn split_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if !matches!(
            ch,
            '\n' | '\r'
                | '\u{0b}'
                | '\u{0c}'
                | '\u{1c}'
                | '\u{1d}'
                | '\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}'
        ) {
            continue;
        }
        out.push(&text[start..index]);
        let mut next = index + ch.len_utf8();
        if ch == '\r'
            && let Some(&(after, '\n')) = chars.peek()
        {
            chars.next();
            next = after + 1;
        }
        start = next;
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

/// Each raw line without its leading timestamp and without ANSI escapes.
#[must_use]
pub fn clean(raw: &[&str]) -> Vec<String> {
    raw.iter()
        .map(|line| {
            let stamped = TIMESTAMP.replace(line, "");
            ANSI.replace_all(&stamped, "").into_owned()
        })
        .collect()
}

fn line_timestamp(raw: &str) -> Option<&str> {
    let stamp = TIMESTAMP.find(raw)?.as_str().trim();
    stamp.get(..19).or(Some(stamp))
}

/// A line's score: 10 a hard error, 6 a strong hint, 3 a weak one, 0 a line
/// that must never anchor (warnings, the exit-code line), -1 nothing.
#[must_use]
pub fn score(line: &str) -> i32 {
    SCORES
        .iter()
        .find(|(_, rx)| rx.is_match(line))
        .map_or(-1, |(value, _)| *value)
}

/// Trim trailing whitespace and cap at [`MAX_LINE_CHARS`] characters.
#[must_use]
pub fn clip(line: &str) -> String {
    let line = line.trim_end();
    if line.chars().count() <= MAX_LINE_CHARS {
        return line.to_owned();
    }
    let mut out: String = line.chars().take(MAX_LINE_CHARS - 1).collect();
    out.push('…');
    out
}

fn norm(text: &str) -> String {
    DIGITS.replace_all(text.trim(), "#").into_owned()
}

fn noise(line: &str) -> bool {
    NOISE.is_match(line)
}

fn blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// `[start, end)` of the failing step in the log, from the step's timestamps;
/// the whole log when the step or its timestamps are unknown.
#[must_use]
pub fn step_window(raw: &[&str], step: Option<&Step>) -> (usize, usize) {
    let whole = (0, raw.len());
    let Some(step) = step else {
        return whole;
    };
    let (Some(start), Some(end)) = (step.started_at.as_deref(), step.completed_at.as_deref())
    else {
        return whole;
    };
    let start = start.get(..19).unwrap_or(start);
    let end = end.get(..19).unwrap_or(end);
    let mut first = None;
    let mut last = None;
    for (index, line) in raw.iter().enumerate() {
        let Some(stamp) = line_timestamp(line) else {
            continue;
        };
        if first.is_none() && stamp >= start {
            first = Some(index);
        }
        if stamp <= end {
            last = Some(index);
        }
    }
    match (first, last) {
        (Some(first), Some(last)) if last >= first => (first, last + 1),
        _ => whole,
    }
}

/// One `CTest` failure: number, name, result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CtestFailure {
    /// Test number in this run.
    pub number: String,
    /// Test name.
    pub name: String,
    /// `Failed`, `Timeout`, ...
    pub result: String,
}

/// The entries of the LAST "The following tests FAILED" block in the window.
#[must_use]
pub fn ctest_failures(lines: &[String], lo: usize, hi: usize) -> Vec<CtestFailure> {
    let Some(index) = (lo..hi).rev().find(|&i| lines[i].contains(FAILED_BLOCK)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in &lines[index + 1..hi.min(index + 401)] {
        if let Some(caps) = FAILED_ENTRY.captures(line) {
            out.push(CtestFailure {
                number: caps[1].to_owned(),
                name: caps[2].trim().to_owned(),
                result: caps[3].to_owned(),
            });
        } else if line.trim().starts_with("Errors while running CTest") && !out.is_empty() {
            break;
        } else if !out.is_empty() && blank(line) {
            break;
        }
    }
    out
}

fn other_failing_tests(lines: &[String], lo: usize, hi: usize) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for line in &lines[lo..hi] {
        let line = line.trim();
        for rx in &*OTHER_FAILURES {
            if let Some(caps) = rx.captures(line) {
                let name = caps[1].to_owned();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names
}

/// One evidence group before it is serialized.
#[derive(Clone, Debug)]
pub struct Group {
    /// `test`, `signal` or `tail`.
    pub kind: &'static str,
    /// 1-based anchor line.
    pub anchor: usize,
    /// Lines.
    pub lines: Vec<String>,
    /// Rank.
    pub score: i32,
    /// Owning test.
    pub test: Option<String>,
    /// Occurrences.
    pub repeats: usize,
    spans: Vec<(usize, usize)>,
    start: usize,
    end: usize,
}

impl Group {
    fn new(kind: &'static str, anchor: usize, lines: Vec<String>, score: i32) -> Self {
        Self {
            kind,
            anchor,
            lines,
            score,
            test: None,
            repeats: 1,
            spans: Vec::new(),
            start: 0,
            end: 0,
        }
    }
}

/// The highest-scoring lines of a contiguous block, in log order, each with
/// the line after it when that line continues it (an assertion's expansion).
fn best_lines(block: &[(usize, &str)], keep: usize) -> Vec<(usize, String)> {
    let Some(&(first, _)) = block.first() else {
        return Vec::new();
    };
    let mut scored: Vec<(i32, usize, &str)> = block
        .iter()
        .filter(|(_, text)| !blank(text) && !noise(text))
        .map(|&(index, text)| (score(text), index, text))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut chosen: BTreeMap<usize, String> = BTreeMap::new();
    for (value, index, text) in scored {
        if value <= 0 || chosen.len() >= keep {
            break;
        }
        chosen.insert(index, text.to_owned());
        if let Some(&(next_index, next_text)) = block.get(index - first + 1)
            && !blank(next_text)
            && chosen.len() < keep
            && score(next_text) < 10
        {
            chosen.insert(next_index, next_text.to_owned());
        }
    }
    chosen.into_iter().collect()
}

fn block_end(lines: &[String], from: usize, hi: usize) -> usize {
    (from + 1..hi.min(from + 1 + TEST_BLOCK_SCAN))
        .find(|&j| ANY_STATUS.is_match(&lines[j]))
        .unwrap_or(hi)
}

fn test_groups(lines: &[String], lo: usize, hi: usize, failures: &[CtestFailure]) -> Vec<Group> {
    let mut by_number: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, line) in lines.iter().enumerate().take(hi).skip(lo) {
        if let Some(caps) = STATUS.captures(line)
            && caps[2].starts_with("***")
        {
            by_number
                .entry(caps.get(1).map_or("", |m| m.as_str()))
                .or_default()
                .push(index);
        }
    }
    let mut groups = Vec::new();
    for failure in failures {
        let Some(status) = by_number.get(failure.number.as_str()) else {
            continue;
        };
        let Some(&at) = status.last() else {
            continue;
        };
        let block: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .take(hi.min(at + 1 + TEST_BLOCK_SCAN))
            .skip(at + 1)
            .take_while(|(_, text)| !ANY_STATUS.is_match(text))
            .map(|(j, text)| (j, text.as_str()))
            .collect();
        let meaningful: Vec<(usize, String)> = block
            .iter()
            .filter(|(_, text)| !blank(text) && !noise(text))
            .map(|&(index, text)| (index, text.to_owned()))
            .collect();
        let picked = if meaningful.len() < MAX_GROUP_LINES {
            meaningful
        } else {
            best_lines(&block, MAX_GROUP_LINES - 1)
        };
        let mut body = vec![clip(lines[at].trim())];
        body.extend(picked.iter().map(|(_, text)| clip(text)));
        let mut group = Group::new("test", at + 1, body, 20);
        group.test = Some(failure.name.clone());
        group.repeats = status.len();
        group.spans = status
            .iter()
            .map(|&k| (k, block_end(lines, k, hi)))
            .collect();
        groups.push(group);
    }
    groups
}

fn signal_groups(
    lines: &[String],
    lo: usize,
    hi: usize,
    exclude: &BTreeSet<usize>,
    floor: i32,
) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for (index, line) in lines.iter().enumerate().take(hi).skip(lo) {
        if exclude.contains(&index)
            || noise(line)
            || score(line) < floor
            || EXIT_CODE.is_match(line.trim())
        {
            continue;
        }
        if let Some(last) = groups.last_mut()
            && index - last.start <= CONTEXT * 2 + 1
        {
            last.score = last.score.max(score(line));
            last.end = index;
            continue;
        }
        let mut group = Group::new("signal", index + 1, Vec::new(), score(line));
        group.start = index;
        group.end = index;
        groups.push(group);
    }
    for group in &mut groups {
        let from = group.start.saturating_sub(CONTEXT).max(lo);
        let to = hi.min(group.end + CONTEXT + 1);
        group.lines = lines[from..to]
            .iter()
            .filter(|text| !blank(text) && !noise(text))
            .take(MAX_GROUP_LINES)
            .map(|text| clip(text.as_str()))
            .collect();
    }
    // Identical messages repeat (a retried test, a lint printed twice).
    let mut merged: Vec<Group> = Vec::new();
    let mut keys: HashMap<String, usize> = HashMap::new();
    for group in groups {
        let key = norm(&group.lines.join("\n"));
        if let Some(&at) = keys.get(&key) {
            merged[at].repeats += 1;
            merged[at].anchor = group.anchor;
        } else {
            keys.insert(key, merged.len());
            merged.push(group);
        }
    }
    merged
}

fn tail_group(lines: &[String], lo: usize, hi: usize) -> Option<Group> {
    let end = (lo..hi)
        .rev()
        .find(|&k| EXIT_CODE.is_match(lines[k].trim()))
        .unwrap_or(hi);
    let mut body: Vec<(usize, &str)> = Vec::new();
    for k in (lo..end).rev() {
        if body.len() >= TAIL_LINES {
            break;
        }
        if !blank(&lines[k]) && !noise(&lines[k]) {
            body.push((k, lines[k].as_str()));
        }
    }
    body.reverse();
    let &(first, _) = body.first()?;
    Some(Group::new(
        "tail",
        first + 1,
        body.iter().map(|(_, text)| clip(text)).collect(),
        5,
    ))
}

/// What the failing step's window says.
#[derive(Clone, Debug, Default)]
pub struct Found {
    /// Failing test names (`CTest` first, else other frameworks' lines).
    pub tests: Vec<String>,
    /// Tests that failed an attempt and passed on retry.
    pub retried: Vec<String>,
    /// Evidence groups, most decisive first.
    pub groups: Vec<Group>,
}

/// Extract tests and evidence groups from the window `[lo, hi)` of cleaned lines.
#[must_use]
pub fn extract(lines: &[String], lo: usize, hi: usize) -> Found {
    if lines.is_empty() {
        return Found::default();
    }
    let failures = ctest_failures(lines, lo, hi);
    let tests: Vec<String> = if failures.is_empty() {
        other_failing_tests(lines, lo, hi)
    } else {
        failures.iter().map(|f| f.name.clone()).collect()
    };
    let mut retried: Vec<String> = Vec::new();
    if !failures.is_empty() {
        let finals: BTreeSet<&str> = failures.iter().map(|f| f.number.as_str()).collect();
        for line in &lines[lo..hi] {
            let Some(caps) = STATUS.captures(line) else {
                continue;
            };
            if !caps[2].starts_with("***") || finals.contains(&caps[1]) {
                continue;
            }
            let after = line.split_once(':').map_or("", |(_, rest)| rest);
            let name = RETRY_NAME_TAIL.replace(after, "").trim().to_owned();
            if !retried.contains(&name) {
                retried.push(name);
            }
        }
    }
    let mut groups = test_groups(lines, lo, hi, &failures);
    let mut covered: BTreeSet<usize> = BTreeSet::new();
    for group in &groups {
        for &(from, to) in &group.spans {
            covered.extend(from..to);
        }
    }
    // The summary block restates failing_tests; never spend evidence on it.
    if !failures.is_empty()
        && let Some(at) = (lo..hi).rev().find(|&i| lines[i].contains(FAILED_BLOCK))
    {
        covered.extend(at.saturating_sub(1)..hi.min(at + 2 + failures.len()));
    }
    // Once tests name the failure, only hard errors elsewhere earn a slot.
    let floor = if groups.is_empty() { 6 } else { 10 };
    let mut signals = signal_groups(lines, lo, hi, &covered, floor);
    signals.sort_by(|a, b| b.score.cmp(&a.score).then(b.anchor.cmp(&a.anchor)));
    if groups.is_empty()
        && let Some(tail) = tail_group(lines, lo, hi)
    {
        let contained = signals.iter().any(|group| {
            let have: BTreeSet<&String> = group.lines.iter().collect();
            tail.lines.iter().all(|line| have.contains(line))
        });
        if !contained {
            let at = signals.len().min(1);
            signals.insert(at, tail);
        }
    }
    groups.extend(signals);
    Found {
        tests,
        retried,
        groups,
    }
}
