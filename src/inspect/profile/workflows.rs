//! Facts read from workflow files: commands, runner classes, release triggers.

use std::collections::BTreeSet;

use regex::Regex;

use super::{Evidence, FactKind, Facts, Look, LookStatus, Snapshot, Source, look, unavailable};
use crate::landability::workflow::{RunsOnResolution, parse_workflow_jobs, resolve_runs_on_expr};

/// Command patterns read from workflow lines, with the value they record.
fn command_patterns() -> Vec<(Regex, &'static str)> {
    let start = r"(?:^|[\s;&|(`])";
    let table: [(&str, &str); 27] = [
        (r"cmake\s", "cmake"),
        (r"ctest(?:\s|$)", "ctest"),
        (r"cargo\s+(?:\+\S+\s+)?build\b", "cargo build"),
        (r"cargo\s+(?:\+\S+\s+)?test\b", "cargo test"),
        (r"cargo\s+(?:\+\S+\s+)?nextest\b", "cargo nextest"),
        (r"cargo\s+(?:\+\S+\s+)?clippy\b", "cargo clippy"),
        (r"cargo\s+(?:\+\S+\s+)?install\b", "cargo install"),
        (r"npm\s+ci\b", "npm ci"),
        (r"npm\s+(?:install|i)\b", "npm install"),
        (r"npm\s+(?:test|run\s+test)\b", "npm test"),
        (r"pnpm\s+(?:install|i)\b", "pnpm install"),
        (r"yarn\s+install\b", "yarn install"),
        (r"(?:python3?\s+-m\s+)?pip3?\s+install\b", "pip install"),
        (r"uv\s+(?:sync|pip|run|tool)\b", "uv"),
        (r"poetry\s+install\b", "poetry install"),
        (r"(?:python3?\s+-m\s+)?pytest\b", "pytest"),
        (r"python3?\s+-m\s+unittest\b", "python -m unittest"),
        (r"go\s+build\b", "go build"),
        (r"go\s+test\b", "go test"),
        (r"(?:\./gradlew|gradle)\s", "gradle"),
        (r"mise\s+(?:install|exec|x|use|run)\b", "mise"),
        (r"brew\s+install\b", "brew install"),
        (r"apt(?:-get)?\s+install\b", "apt-get install"),
        (r"(?:npx\s+)?jest\b", "jest"),
        (r"(?:npx\s+)?vitest\b", "vitest"),
        (r"swift\s+(?:build|test)\b", "swift"),
        (r"xcodebuild\b", "xcodebuild"),
    ];
    let mut patterns: Vec<(Regex, &'static str)> = table
        .iter()
        .map(|(pattern, value)| {
            (
                Regex::new(&format!("{start}{pattern}")).expect("command pattern"),
                *value,
            )
        })
        .collect();
    patterns.push((
        Regex::new(
            r"uses:\s*['\x22]?(actions/cache|actions/setup-[a-z]+|Swatinem/rust-cache|jdx/mise-action|astral-sh/setup-uv)@",
        )
        .expect("action pattern"),
        "uses",
    ));
    patterns
}

/// Labels of a `runs-on:` that GitHub hosts.
fn hosted_label(label: &str) -> bool {
    ["ubuntu-", "macos-", "windows-"]
        .iter()
        .any(|prefix| label.starts_with(prefix))
}

/// The labels of a literal `runs-on:` value.
fn literal_labels(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|label| {
            label
                .trim()
                .trim_matches(|c| c == '"' || c == '\'')
                .to_owned()
        })
        .filter(|label| !label.is_empty())
        .collect()
}

/// The class of a set of runner labels.
fn label_class(labels: &[String]) -> String {
    if labels.iter().any(|label| label == "self-hosted") {
        format!("self-hosted [{}]", labels.join(", "))
    } else if !labels.is_empty() && labels.iter().all(|label| hosted_label(label)) {
        format!("github-hosted [{}]", labels.join(", "))
    } else {
        format!("custom labels [{}]", labels.join(", "))
    }
}

/// The `group:`/`labels:` mapping form of `runs-on:`, joined onto one line.
fn group_form(expr: &str) -> Option<String> {
    let rest = expr.trim();
    if !rest.starts_with("group:") && !rest.starts_with("labels:") {
        return None;
    }
    let (group, labels) = match rest.split_once("labels:") {
        Some((group, labels)) => (group, labels),
        None => (rest, ""),
    };
    let group = group.trim().trim_start_matches("group:").trim();
    // Labels are a `[a, b]` list or `- a - b` items; a label has no spaces.
    let labels: Vec<String> = if labels.trim_start().starts_with('[') {
        literal_labels(labels)
    } else {
        labels
            .split_whitespace()
            .filter(|token| *token != "-")
            .map(|token| token.trim_matches(|c| c == '"' || c == '\'').to_owned())
            .collect()
    };
    let class = label_class(&labels);
    Some(if group.is_empty() {
        class
    } else {
        format!("{class} (group {group})")
    })
}

/// The runner class a `runs-on:` asks for.
pub(super) fn static_runner_class(expr: &str) -> String {
    if let Some(class) = group_form(expr) {
        return class;
    }
    match resolve_runs_on_expr(expr) {
        RunsOnResolution::Literal { value } => label_class(&literal_labels(&value)),
        RunsOnResolution::Variable { name, fallback } => match fallback {
            Some(fallback) => format!("routed by vars.{name} (unset: {fallback})"),
            None => format!("routed by vars.{name}"),
        },
        RunsOnResolution::Dynamic { origin, .. } => format!("computed from {origin}"),
        RunsOnResolution::Unparsable { raw } => format!("computed: {}", raw.trim()),
    }
}

/// Whether a `runs-on:` is computed, so only a run shows where it landed.
#[must_use]
pub fn runs_on_is_computed(expr: &str) -> bool {
    group_form(expr).is_none()
        && !matches!(resolve_runs_on_expr(expr), RunsOnResolution::Literal { .. })
}

/// The line of each job's `runs-on:`, in job order.
fn runs_on_lines(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let trimmed = line.trim_start();
            trimmed.starts_with("runs-on:") && line.len() - trimmed.len() >= 2
        })
        .map(|(index, _)| index + 1)
        .collect()
}

/// The `on:` block's lines, with their numbers.
fn on_block(text: &str) -> Vec<(usize, &str)> {
    let mut block = Vec::new();
    let mut inside = false;
    for (index, line) in text.lines().enumerate() {
        let top_level = !line.starts_with(' ')
            && !line.starts_with('\t')
            && !line.trim().is_empty()
            && !line.trim_start().starts_with('#');
        if top_level {
            let key = line
                .split(':')
                .next()
                .unwrap_or_default()
                .trim()
                .trim_matches(['"', '\'']);
            inside = key == "on" || key == "true";
            if inside {
                block.push((index + 1, line));
            }
            continue;
        }
        if inside {
            block.push((index + 1, line));
        }
    }
    block
}

const WORKFLOW_KINDS: [(FactKind, &str); 3] = [
    (
        FactKind::CiCommand,
        "build, install, test and setup commands on workflow lines",
    ),
    (FactKind::RunnerClass, "every job's runs-on:"),
    (
        FactKind::ReleaseWorkflow,
        "on: triggers for tags or releases",
    ),
];

pub(super) fn workflow_facts(snapshot: &Snapshot, facts: &mut Facts, looks: &mut Vec<Look>) {
    let workflows = match &snapshot.workflows {
        Ok(workflows) => workflows,
        Err(error) => {
            for (kind, what) in WORKFLOW_KINDS {
                looks.push(unavailable(
                    kind,
                    Source::Workflow,
                    format!("{what} in .github/workflows"),
                    error,
                ));
            }
            return;
        }
    };
    let patterns = command_patterns();
    let mut unreadable: Vec<String> = Vec::new();
    for workflow in workflows {
        let Some(text) = &workflow.text else {
            unreadable.push(format!(
                "{} ({})",
                workflow.path,
                workflow.note.as_deref().unwrap_or("no text")
            ));
            continue;
        };
        command_facts(&workflow.path, text, &patterns, facts);
        runner_facts(&workflow.path, text, facts);
        release_fact(&workflow.path, text, facts);
    }
    for (kind, what) in WORKFLOW_KINDS {
        let mut entry = look(
            kind,
            Source::Workflow,
            facts.count(kind, Source::Workflow),
            format!("{what} in {} workflow files", workflows.len()),
        );
        if !unreadable.is_empty() {
            entry.status = LookStatus::Partial;
            entry.note = Some(format!("text not read: {}", unreadable.join(", ")));
        }
        looks.push(entry);
    }
}

fn command_facts(path: &str, text: &str, patterns: &[(Regex, &'static str)], facts: &mut Facts) {
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#')
            || trimmed.starts_with("name:")
            || trimmed.starts_with("- name:")
        {
            continue;
        }
        let mut seen = BTreeSet::new();
        for (pattern, value) in patterns {
            let Some(found) = pattern.captures(line) else {
                continue;
            };
            let value = if *value == "uses" {
                format!("uses {}", found.get(1).map_or("", |action| action.as_str()))
            } else {
                (*value).to_owned()
            };
            if seen.insert(value.clone()) {
                facts.add(
                    FactKind::CiCommand,
                    &value,
                    Source::Workflow,
                    false,
                    Evidence {
                        at: format!("{path}:{}", index + 1),
                        text: trimmed.chars().take(160).collect(),
                    },
                );
            }
        }
    }
}

fn runner_facts(path: &str, text: &str, facts: &mut Facts) {
    let jobs = parse_workflow_jobs(text);
    let lines = runs_on_lines(text);
    let mut lines = lines.into_iter();
    for job in jobs.iter().filter(|job| job.runs_on_expr.is_some()) {
        let expr = job.runs_on_expr.as_deref().unwrap_or_default();
        let at = lines
            .next()
            .map_or_else(|| path.to_owned(), |line| format!("{path}:{line}"));
        facts.add(
            FactKind::RunnerClass,
            &static_runner_class(expr),
            Source::Workflow,
            false,
            Evidence {
                at,
                text: format!("job {}: runs-on: {}", job.id, expr.trim()),
            },
        );
    }
}

fn release_fact(path: &str, text: &str, facts: &mut Facts) {
    for (line, text) in on_block(text) {
        let trimmed = text.trim();
        let releases = trimmed.starts_with("tags:")
            || trimmed.starts_with("release:")
            || trimmed.starts_with("- release")
            || (trimmed.starts_with("on:") && trimmed.contains("release"));
        if releases {
            facts.add(
                FactKind::ReleaseWorkflow,
                path,
                Source::Workflow,
                false,
                Evidence {
                    at: format!("{path}:{line}"),
                    text: trimmed.to_owned(),
                },
            );
            return;
        }
    }
}
