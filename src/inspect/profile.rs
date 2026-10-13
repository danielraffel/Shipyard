//! The detected project profile: facts read from a repository, with evidence.
//!
//! A check applies because of what the project actually does, never because
//! of a generic list. This module reads those facts:
//!
//! - build systems, from the marker files of `shipyard init`'s ecosystem
//!   registry (plus `pyproject.toml`, and Python sources with no manifest);
//! - lockfiles and pin manifests;
//! - the commands and setup actions the workflows run;
//! - runner classes, both as written (`runs-on:`) and as observed (the
//!   runner of a recent job, for every job whose `runs-on:` is computed);
//! - the merge queue and the required checks of the base branch;
//! - test frameworks, from files and from the commands CI runs;
//! - release workflows, from their `on:` triggers.
//!
//! Every fact carries the file and line, or the API row, it came from. A
//! kind with no fact says what was searched; a kind whose read failed says
//! so and is never reported as absent. Paths under fixtures, tests,
//! examples, vendored or experimental directories are kept but marked
//! `incidental`, so a fixture's `package.json` does not make a project a web
//! project.
//!
//! Labels (`desktop-native`, `rust-cli`, `python-tool`, `web`) are names for
//! bundles of these facts and nothing more: checks read the facts.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;
use serde_json::Value;

pub mod gather;
mod settings;
mod tree;
mod workflows;

pub use workflows::runs_on_is_computed;
#[cfg(test)]
use workflows::static_runner_class;

/// Report schema identifier.
pub const SCHEMA: &str = "shipyard-inspect-profile/v1";
/// Evidence rows kept per fact; `occurrences` counts them all.
const EVIDENCE_CAP: usize = 5;
/// Directory names whose contents are incidental to what the project builds.
const INCIDENTAL_DIRS: [&str; 16] = [
    "fixtures",
    "fixture",
    "test",
    "tests",
    "testdata",
    "examples",
    "example",
    "samples",
    "sample",
    "vendor",
    "vendored",
    "third_party",
    "third-party",
    "external",
    "node_modules",
    "experimental",
];
/// Lockfiles: an exact resolution of a dependency graph.
const LOCKFILES: [&str; 16] = [
    "Cargo.lock",
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lockb",
    "bun.lock",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "go.sum",
    "gradle.lockfile",
    "Package.resolved",
    "pubspec.lock",
    "composer.lock",
    "Gemfile.lock",
];
/// Pin manifests: tool versions, or dependency pins without a full graph.
const PIN_MANIFESTS: [&str; 9] = [
    "mise.toml",
    ".mise.toml",
    ".tool-versions",
    "rust-toolchain",
    "rust-toolchain.toml",
    ".python-version",
    ".nvmrc",
    ".node-version",
    ".go-version",
];
/// Build markers `shipyard init`'s registry does not carry.
const EXTRA_MARKERS: [(&str, &str, &str); 2] = [
    ("pyproject.toml", "python", "python-pyproject"),
    ("setup.cfg", "python", "python-setuptools"),
];

/// What a fact is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FactKind {
    /// A build system, by ecosystem family.
    BuildSystem,
    /// A lockfile.
    Lockfile,
    /// A tool-version or dependency pin manifest.
    PinManifest,
    /// A command or setup action a workflow runs.
    CiCommand,
    /// A class of runner jobs ask for or ran on.
    RunnerClass,
    /// The base branch's merge queue.
    MergeQueue,
    /// A check the base branch requires.
    RequiredCheck,
    /// A test framework.
    TestFramework,
    /// A workflow that releases.
    ReleaseWorkflow,
}

impl FactKind {
    /// Every kind, in report order.
    pub const ALL: [Self; 9] = [
        Self::BuildSystem,
        Self::Lockfile,
        Self::PinManifest,
        Self::CiCommand,
        Self::RunnerClass,
        Self::MergeQueue,
        Self::RequiredCheck,
        Self::TestFramework,
        Self::ReleaseWorkflow,
    ];

    /// Plural heading for the human report.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::BuildSystem => "build systems",
            Self::Lockfile => "lockfiles",
            Self::PinManifest => "pin manifests",
            Self::CiCommand => "CI commands",
            Self::RunnerClass => "runner classes",
            Self::MergeQueue => "merge queue",
            Self::RequiredCheck => "required checks",
            Self::TestFramework => "test frameworks",
            Self::ReleaseWorkflow => "release workflows",
        }
    }
}

/// Where a fact was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The repository's file list.
    Tree,
    /// A workflow file's text.
    Workflow,
    /// A repository settings API (rules, branch protection).
    Settings,
    /// A recent run's jobs.
    Runs,
}

/// One place a fact was read from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Evidence {
    /// `path:line`, a path, or the API row (`api:<endpoint>`, `run <id>`).
    pub at: String,
    /// What is there.
    pub text: String,
}

/// One detected fact.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fact {
    /// What it is about.
    pub kind: FactKind,
    /// The fact itself (`rust`, `Cargo.lock`, `cargo test`, ...).
    pub value: String,
    /// Where it was read.
    pub source: Source,
    /// Every place it was read lies under an incidental directory.
    pub incidental: bool,
    /// How many places it was read from.
    pub occurrences: usize,
    /// Up to five of those places.
    pub evidence: Vec<Evidence>,
}

/// The outcome of looking for one kind of fact in one source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LookStatus {
    /// At least one fact.
    Found,
    /// The source was read in full and holds none.
    NoneFound,
    /// Some of the source was read; the facts are a lower bound.
    Partial,
    /// The source could not be read; nothing is known.
    Unavailable,
}

/// What was searched for one kind in one source, and with what result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Look {
    /// The kind looked for.
    pub kind: FactKind,
    /// Where it was looked for.
    pub source: Source,
    /// The result.
    pub status: LookStatus,
    /// What was searched, in words.
    pub searched: String,
    /// Why it is partial or unavailable, or a limit worth knowing.
    pub note: Option<String>,
}

/// A name for a bundle of facts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Label {
    /// `desktop-native`, `rust-cli`, `python-tool` or `web`.
    pub name: &'static str,
    /// The facts it names, as `kind value (where)`.
    pub because: Vec<String>,
}

/// The detected profile of one repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProjectProfile {
    /// [`SCHEMA`].
    pub schema: &'static str,
    /// `OWNER/REPO`.
    pub repo: String,
    /// Branch read.
    pub base: String,
    /// Commit the files were read at.
    pub commit: Option<String>,
    /// Labels the facts support.
    pub labels: Vec<Label>,
    /// `[inspect] profile` from config, when set. Shown beside the detected
    /// labels; it never adds a fact.
    pub label_override: Option<String>,
    /// Every fact, by kind.
    pub facts: Vec<Fact>,
    /// What was searched for each kind, including the kinds with no fact.
    pub looks: Vec<Look>,
    /// API calls the read spent.
    pub api_calls: u32,
}

impl ProjectProfile {
    /// Facts of one kind.
    pub fn facts_of(&self, kind: FactKind) -> impl Iterator<Item = &Fact> {
        self.facts.iter().filter(move |fact| fact.kind == kind)
    }

    /// Whether a non-incidental fact of `kind` has exactly `value`.
    #[must_use]
    pub fn has(&self, kind: FactKind, value: &str) -> bool {
        self.facts_of(kind)
            .any(|fact| !fact.incidental && fact.value == value)
    }
}

/// A workflow file as read: its text, or why there is none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkflowFile {
    /// Repository path.
    pub path: String,
    /// The text, when it could be read whole.
    pub text: Option<String>,
    /// Why the text is missing (binary, truncated).
    pub note: Option<String>,
}

/// The jobs of one recent run of a workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct SampledRun {
    /// Workflow file name.
    pub workflow: String,
    /// Run id.
    pub run_id: u64,
    /// Job objects as GitHub returns them.
    pub jobs: Vec<Value>,
    /// The run listed more jobs than one page returned.
    pub partial: bool,
}

/// The runs read to observe where computed `runs-on:` values landed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RunSample {
    /// Workflows whose `runs-on:` is computed and so needed a run.
    pub wanted: Vec<String>,
    /// Their runs, as read.
    pub sampled: Vec<SampledRun>,
    /// Workflows left unread by the budget.
    pub over_budget: Vec<String>,
    /// Workflows that had no completed run.
    pub no_runs: Vec<String>,
    /// Workflows whose read failed, with the error.
    pub errors: Vec<(String, String)>,
}

/// Everything the profile is computed from, as read.
#[derive(Clone, Debug, PartialEq)]
pub struct Snapshot {
    /// Commit the base resolved to.
    pub commit: Result<String, String>,
    /// Blob paths, and whether GitHub truncated the list.
    pub paths: Result<(Vec<String>, bool), String>,
    /// Workflow files.
    pub workflows: Result<Vec<WorkflowFile>, String>,
    /// `rules/branches/<base>`.
    pub rules: Result<Value, String>,
    /// Classic required-status-check protection; `None` when the branch has
    /// none.
    pub protection: Result<Option<Value>, String>,
    /// Recent runs of the workflows whose runner is computed.
    pub runs: RunSample,
}

/// Collects facts keyed by (kind, value), merging evidence.
#[derive(Default)]
struct Facts {
    by_key: BTreeMap<(FactKind, String), Fact>,
}

impl Facts {
    fn add(
        &mut self,
        kind: FactKind,
        value: &str,
        source: Source,
        incidental: bool,
        evidence: Evidence,
    ) {
        let fact = self
            .by_key
            .entry((kind, value.to_owned()))
            .or_insert_with(|| Fact {
                kind,
                value: value.to_owned(),
                source,
                incidental: true,
                occurrences: 0,
                evidence: Vec::new(),
            });
        fact.occurrences += 1;
        fact.incidental &= incidental;
        // Non-incidental evidence first, so the first row shown is the one
        // that decides.
        if !incidental && fact.evidence.len() == EVIDENCE_CAP {
            fact.evidence.pop();
        }
        if fact.evidence.len() < EVIDENCE_CAP {
            let position = if incidental {
                fact.evidence.len()
            } else {
                fact.evidence
                    .iter()
                    .position(|row| is_incidental(row.at.split(':').next().unwrap_or_default()))
                    .unwrap_or(fact.evidence.len())
            };
            fact.evidence.insert(position, evidence);
        }
    }

    fn count(&self, kind: FactKind, source: Source) -> usize {
        self.by_key
            .values()
            .filter(|fact| fact.kind == kind && fact.source == source)
            .count()
    }
}

/// Whether `path` lies under a directory incidental to the build.
#[must_use]
pub fn is_incidental(path: &str) -> bool {
    let mut parts: Vec<&str> = path.split('/').collect();
    parts.pop();
    parts.iter().any(|part| {
        INCIDENTAL_DIRS
            .iter()
            .any(|dir| part.eq_ignore_ascii_case(dir))
    })
}

/// `name` ends with `suffix`, ignoring ASCII case.
fn has_suffix(name: &str, suffix: &str) -> bool {
    name.len() >= suffix.len()
        && name.is_char_boundary(name.len() - suffix.len())
        && name[name.len() - suffix.len()..].eq_ignore_ascii_case(suffix)
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn look(kind: FactKind, source: Source, found: usize, searched: String) -> Look {
    Look {
        kind,
        source,
        status: if found > 0 {
            LookStatus::Found
        } else {
            LookStatus::NoneFound
        },
        searched,
        note: None,
    }
}

fn unavailable(kind: FactKind, source: Source, searched: String, error: &str) -> Look {
    Look {
        kind,
        source,
        status: LookStatus::Unavailable,
        searched,
        note: Some(error.to_owned()),
    }
}

/// Compute the profile from what was read.
#[must_use]
pub fn detect(
    repo: &str,
    base: &str,
    snapshot: &Snapshot,
    api_calls: u32,
    label_override: Option<String>,
) -> ProjectProfile {
    let mut facts = Facts::default();
    let mut looks = Vec::new();
    let paths: Option<&Vec<String>> = snapshot.paths.as_ref().ok().map(|(paths, _)| paths);

    tree::tree_facts(snapshot, &mut facts, &mut looks);
    workflows::workflow_facts(snapshot, &mut facts, &mut looks);
    settings::runtime_runner_facts(&snapshot.runs, &mut facts, &mut looks);
    settings::settings_facts(repo, base, snapshot, &mut facts, &mut looks);
    tree::test_framework_facts(paths, &mut facts, &mut looks, snapshot);

    let labels = labels(&facts, paths);
    let mut ordered: Vec<Fact> = facts.by_key.into_values().collect();
    ordered.sort_by(|left, right| {
        (
            left.kind,
            left.incidental,
            std::cmp::Reverse(left.occurrences),
            &left.value,
        )
            .cmp(&(
                right.kind,
                right.incidental,
                std::cmp::Reverse(right.occurrences),
                &right.value,
            ))
    });
    ProjectProfile {
        schema: SCHEMA,
        repo: repo.to_owned(),
        base: base.to_owned(),
        commit: snapshot.commit.as_ref().ok().cloned(),
        labels,
        label_override,
        facts: ordered,
        looks,
        api_calls,
    }
}

fn root_has(paths: Option<&Vec<String>>, name: &str) -> bool {
    paths.is_some_and(|paths| paths.iter().any(|path| path == name))
}

fn labels(facts: &Facts, paths: Option<&Vec<String>>) -> Vec<Label> {
    let fact = |kind: FactKind, value: &str| {
        facts
            .by_key
            .get(&(kind, value.to_owned()))
            .filter(|fact| !fact.incidental)
    };
    let commands = |names: &[&str]| -> Vec<String> {
        names
            .iter()
            .filter_map(|name| fact(FactKind::CiCommand, name))
            .map(|fact| format!("ci_command {} ({})", fact.value, fact.evidence[0].at))
            .collect()
    };
    let mut labels = Vec::new();

    if root_has(paths, "CMakeLists.txt") {
        // Only a runner the job asks for by its labels, or one it ran on: a
        // computed or routed class says nothing about the OS until a run does.
        let desktop: Vec<String> = facts
            .by_key
            .values()
            .filter(|fact| {
                let value = fact.value.to_ascii_lowercase();
                fact.kind == FactKind::RunnerClass
                    && (value.starts_with("github-hosted") || value.starts_with("self-hosted"))
                    && (value.contains("macos")
                        || value.contains("windows")
                        || value.contains("local-mac"))
            })
            .map(|fact| format!("runner_class {} ({})", fact.value, fact.evidence[0].at))
            .collect();
        if !desktop.is_empty() {
            let mut because = vec!["build_system cpp (CMakeLists.txt)".to_owned()];
            because.extend(desktop.into_iter().take(2));
            labels.push(Label {
                name: "desktop-native",
                because,
            });
        }
    }
    if root_has(paths, "Cargo.toml") {
        let entry = paths.and_then(|paths| {
            paths
                .iter()
                .find(|path| *path == "src/main.rs")
                .or_else(|| paths.iter().find(|path| path.starts_with("src/bin/")))
        });
        if let Some(entry) = entry {
            labels.push(Label {
                name: "rust-cli",
                because: vec![
                    "build_system rust (Cargo.toml)".to_owned(),
                    format!("binary entry point ({entry})"),
                ],
            });
        }
    }
    let other_root = ["CMakeLists.txt", "Cargo.toml", "package.json", "go.mod"]
        .iter()
        .any(|name| root_has(paths, name));
    if let Some(python) = fact(FactKind::BuildSystem, "python").filter(|_| !other_root) {
        let python_ci = commands(&[
            "pytest",
            "python -m unittest",
            "pip install",
            "uv",
            "poetry install",
        ]);
        if !python_ci.is_empty() {
            let mut because = vec![format!("build_system python ({})", python.evidence[0].at)];
            because.extend(python_ci);
            labels.push(Label {
                name: "python-tool",
                because,
            });
        }
    }
    if let Some(node) = fact(FactKind::BuildSystem, "node") {
        let node_ci = commands(&["npm ci", "npm install", "pnpm install", "yarn install"]);
        if !node_ci.is_empty() {
            let mut because = vec![format!("build_system node ({})", node.evidence[0].at)];
            because.extend(node_ci);
            labels.push(Label {
                name: "web",
                because,
            });
        }
    }
    labels
}

/// The human report.
#[must_use]
pub fn render(profile: &ProjectProfile) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Project profile: {} at {} ({})",
        profile.repo,
        profile.base,
        profile.commit.as_deref().map_or_else(
            || "commit unread".to_owned(),
            |commit| commit.chars().take(12).collect()
        )
    );
    if profile.labels.is_empty() {
        let _ = writeln!(out, "labels: none (no label's facts are all present)");
    }
    for label in &profile.labels {
        let _ = writeln!(out, "label {}: {}", label.name, label.because.join("; "));
    }
    if let Some(name) = &profile.label_override {
        let _ = writeln!(
            out,
            "label set by [inspect] profile: {name} (the facts below are what checks read)"
        );
    }
    for kind in FactKind::ALL {
        let _ = writeln!(out, "\n{}:", kind.title());
        for fact in profile.facts_of(kind) {
            let first = fact
                .evidence
                .first()
                .map_or_else(String::new, |row| row.at.clone());
            let more = if fact.occurrences > 1 {
                format!(" (+{} more)", fact.occurrences - 1)
            } else {
                String::new()
            };
            let incidental = if fact.incidental { " [incidental]" } else { "" };
            let _ = writeln!(out, "  {}{incidental}  {first}{more}", fact.value);
        }
        for entry in profile.looks.iter().filter(|entry| entry.kind == kind) {
            let status = match entry.status {
                LookStatus::Found => continue_marker(entry),
                LookStatus::NoneFound => "none found".to_owned(),
                LookStatus::Partial => "PARTIAL".to_owned(),
                LookStatus::Unavailable => "UNAVAILABLE".to_owned(),
            };
            if status.is_empty() {
                continue;
            }
            let note = entry
                .note
                .as_deref()
                .map_or_else(String::new, |note| format!(": {note}"));
            let _ = writeln!(out, "  ({status}; searched {}{note})", entry.searched);
        }
    }
    let _ = writeln!(out, "\napi calls: {}", profile.api_calls);
    out
}

fn continue_marker(entry: &Look) -> String {
    if entry.note.is_some() {
        "note".to_owned()
    } else {
        String::new()
    }
}

#[cfg(test)]
mod tests;
