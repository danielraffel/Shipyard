//! Check-run annotations from a diagnosis: each failing test's evidence on the
//! repository file and line it names, so the failure reads in context with no
//! log.
//!
//! A position is posted only when its path resolves to exactly one file in the
//! head's tree: an exact repository-relative match after the runner workspace
//! prefix is stripped, else a unique path-suffix match. Standard-library
//! frames, tool caches, ambiguous suffixes and files named without a line are
//! dropped, never guessed.
//!
//! Only what the failure IS gets a position: each finally-failing test's group,
//! or, when no test named the failure, the root group and the step's tail (a
//! lint's or guard's verdict). A signal group elsewhere in the step (a test
//! that failed once and passed on retry) is evidence, not a verdict on the
//! line it names. Causes that are the runner or the run (`no_runner`,
//! `needs_starved`, `lost_runner`, `non_content_step`, every `interrupted`)
//! get no position at all.
//!
//! The annotations ride on a separate, always-`neutral` check run: the
//! diagnosis annotates a red, it never becomes one.

use std::collections::{BTreeSet, HashMap};
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::Diagnosis;
use super::classify::{FLAKE, INTERRUPTED, REAL};

/// GitHub's per-request annotation limit.
pub const MAX_ANNOTATIONS: usize = 50;
/// Longest annotation message, in characters.
pub const MAX_MESSAGE: usize = 1_000;
/// Name of the check run that carries the annotations.
pub const CHECK_RUN_NAME: &str = "shipyard diagnose";

const NO_POSITION_RULES: [&str; 4] = [
    "no_runner",
    "needs_starved",
    "lost_runner",
    "non_content_step",
];

fn pattern(source: &str) -> Regex {
    Regex::new(source).expect("diagnose annotate pattern compiles")
}

/// `path:line` in the forms CI tools print. The last form requires the
/// position to be followed by `:`, whitespace, `)` or the end of the line;
/// [`positions`] checks that, since the regex engine has no lookahead.
static POSITIONS: LazyLock<[Regex; 4]> = LazyLock::new(|| {
    [
        pattern(r"CMake Error at (?P<path>[^\s:()]+):(?P<line>\d+)"),
        pattern(r#"File "(?P<path>[^"]+)", line (?P<line>\d+)"#),
        pattern(r"panicked at (?P<path>[^\s:]+):(?P<line>\d+)"),
        pattern(
            r#"(?:^|[\s("'`])(?P<path>[\w./+-]*[\w+-]\.[A-Za-z]\w{0,9}):(?P<line>\d+)(?P<col>:\d+)?"#,
        ),
    ]
});
static WORKSPACES: LazyLock<[Regex; 3]> = LazyLock::new(|| {
    [
        pattern(r"^.*?/_work/[^/]+/[^/]+/"),
        pattern(r"^/home/runner/work/[^/]+/[^/]+/"),
        pattern(r"^[A-Za-z]:\\a\\[^\\]+\\[^\\]+\\"),
    ]
});
static FOREIGN: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"^(?:/opt/hostedtoolcache/|/usr/|/Library/|/System/|<)"));

/// The paths of the head's tree, indexed by file name.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    paths: BTreeSet<String>,
    by_name: HashMap<String, Vec<String>>,
}

impl Tree {
    /// Index a list of repository-relative paths.
    #[must_use]
    pub fn new<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let paths: BTreeSet<String> = paths.into_iter().map(Into::into).collect();
        let mut by_name: HashMap<String, Vec<String>> = HashMap::new();
        for path in &paths {
            by_name
                .entry(file_name(path).to_owned())
                .or_default()
                .push(path.clone());
        }
        Self { paths, by_name }
    }

    /// Whether the tree is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.paths.is_empty()
    }

    /// The one tree path a printed path names, or `None`.
    #[must_use]
    pub fn resolve(&self, raw: &str) -> Option<String> {
        let mut path = raw.replace('\\', "/");
        if FOREIGN.is_match(&path) {
            return None;
        }
        for rx in &*WORKSPACES {
            path = rx.replace(&path, "").into_owned();
        }
        if path.starts_with("./") {
            path = path.trim_start_matches(['.', '/']).to_owned();
        }
        if self.paths.contains(&path) {
            return Some(path);
        }
        let path = path.trim_start_matches('/');
        let suffix = format!("/{path}");
        let hits: Vec<&String> = self
            .by_name
            .get(file_name(path))
            .into_iter()
            .flatten()
            .filter(|candidate| candidate.as_str() == path || candidate.ends_with(&suffix))
            .collect();
        match hits.as_slice() {
            [only] => Some((*only).clone()),
            _ => None,
        }
    }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Every `(path, line)` a line prints, in pattern order.
#[must_use]
pub fn positions(line: &str) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for (index, rx) in POSITIONS.iter().enumerate() {
        for caps in rx.captures_iter(line) {
            let (Some(path), Some(number)) = (caps.name("path"), caps.name("line")) else {
                continue;
            };
            if index == 3 {
                // Lookahead by hand: the position must end at `:`, whitespace,
                // `)` or the end of the line, with or without its column.
                let ends_ok = |at: usize| {
                    line[at..]
                        .chars()
                        .next()
                        .is_none_or(|ch| ch == ':' || ch == ')' || ch.is_whitespace())
                };
                let with_col = caps.name("col").is_some_and(|col| ends_ok(col.end()));
                if !with_col && !ends_ok(number.end()) {
                    continue;
                }
            }
            if let Ok(number) = number.as_str().parse::<u32>() {
                out.push((path.as_str().to_owned(), number));
            }
        }
    }
    out
}

/// One check-run annotation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Annotation {
    /// Repository-relative path.
    pub path: String,
    /// First line.
    pub start_line: u32,
    /// Last line.
    pub end_line: u32,
    /// `failure` for a real failure, `warning` for a flake candidate, else
    /// `notice`.
    pub annotation_level: String,
    /// `<check>: <test or class>`.
    pub title: String,
    /// The evidence group's lines.
    pub message: String,
}

/// The annotations a diagnosis supports, at most [`MAX_ANNOTATIONS`].
#[must_use]
pub fn annotations(doc: &Diagnosis, tree: &Tree) -> Vec<Annotation> {
    let mut out: Vec<Annotation> = Vec::new();
    let mut seen: BTreeSet<(String, u32)> = BTreeSet::new();
    for check in &doc.checks {
        let class = check.classification.class.as_str();
        if class == INTERRUPTED || NO_POSITION_RULES.contains(&check.classification.rule.as_str()) {
            continue;
        }
        let tests: Vec<_> = check.evidence.iter().filter(|g| g.kind == "test").collect();
        let groups = if tests.is_empty() {
            check
                .evidence
                .iter()
                .enumerate()
                .filter(|(index, group)| *index == 0 || group.kind == "tail")
                .map(|(_, group)| group)
                .collect()
        } else {
            tests
        };
        for group in groups {
            let position = group.lines.iter().find_map(|line| {
                positions(line).into_iter().find_map(|(raw, number)| {
                    let path = tree.resolve(&raw)?;
                    (number > 0).then_some((path, number))
                })
            });
            let Some(position) = position else {
                continue;
            };
            if !seen.insert(position.clone()) {
                continue;
            }
            let level = match class {
                REAL => "failure",
                FLAKE => "warning",
                _ => "notice",
            };
            let title = format!(
                "{}: {}",
                check.context,
                group.test.as_deref().unwrap_or(class)
            );
            out.push(Annotation {
                path: position.0,
                start_line: position.1,
                end_line: position.1,
                annotation_level: level.to_owned(),
                title: title.chars().take(255).collect(),
                message: group.lines.join("\n").chars().take(MAX_MESSAGE).collect(),
            });
            if out.len() >= MAX_ANNOTATIONS {
                return out;
            }
        }
    }
    out
}

/// Output block of the check run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckRunOutput {
    /// The diagnosis summary, capped.
    pub title: String,
    /// The diagnosis summary.
    pub summary: String,
    /// Positions.
    pub annotations: Vec<Annotation>,
}

/// The body of `POST /repos/{owner}/{repo}/check-runs`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct CheckRunPayload {
    /// [`CHECK_RUN_NAME`].
    pub name: String,
    /// Commit the annotations belong to.
    pub head_sha: String,
    /// Always `completed`.
    pub status: String,
    /// Always `neutral`.
    pub conclusion: String,
    /// Summary and annotations.
    pub output: CheckRunOutput,
}

/// The check-run body for a diagnosis on `head_sha`.
#[must_use]
pub fn check_run_payload(doc: &Diagnosis, head_sha: &str, tree: &Tree) -> CheckRunPayload {
    CheckRunPayload {
        name: CHECK_RUN_NAME.to_owned(),
        head_sha: head_sha.to_owned(),
        status: "completed".to_owned(),
        conclusion: "neutral".to_owned(),
        output: CheckRunOutput {
            title: doc.summary.chars().take(255).collect(),
            summary: doc.summary.chars().take(65_000).collect(),
            annotations: annotations(doc, tree),
        },
    }
}
