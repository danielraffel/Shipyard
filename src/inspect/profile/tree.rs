//! Facts read from the repository's file list.

use std::collections::BTreeSet;

use super::{
    EVIDENCE_CAP, EXTRA_MARKERS, Evidence, FactKind, Facts, LOCKFILES, Look, LookStatus,
    PIN_MANIFESTS, Snapshot, Source, file_name, has_suffix, is_incidental, look, unavailable,
};

const TREE_KINDS: [(FactKind, &str); 3] = [
    (
        FactKind::BuildSystem,
        "build markers of every ecosystem shipyard init knows, and *.py sources",
    ),
    (
        FactKind::Lockfile,
        "lockfiles (Cargo.lock, package-lock.json, uv.lock, ...)",
    ),
    (
        FactKind::PinManifest,
        "pin manifests (mise.toml, rust-toolchain.toml, requirements*.txt, ...)",
    ),
];

pub(super) fn tree_facts(snapshot: &Snapshot, facts: &mut Facts, looks: &mut Vec<Look>) {
    let (paths, truncated) = match &snapshot.paths {
        Ok((paths, truncated)) if !paths.is_empty() => (paths, *truncated),
        Ok(_) => {
            for (kind, what) in TREE_KINDS {
                looks.push(unavailable(
                    kind,
                    Source::Tree,
                    format!("{what} in the file list"),
                    "the file list read returned no files, so absence cannot be told from a \
                     broken read",
                ));
            }
            return;
        }
        Err(error) => {
            for (kind, what) in TREE_KINDS {
                looks.push(unavailable(
                    kind,
                    Source::Tree,
                    format!("{what} in the file list"),
                    error,
                ));
            }
            return;
        }
    };
    let markers: Vec<(&str, &str, &str)> = crate::init_config::ecosystem_markers()
        .into_iter()
        .chain(EXTRA_MARKERS)
        .collect();
    let mut python_sources: Vec<&String> = Vec::new();
    let mut python_manifest = false;
    for path in paths {
        python_manifest |= path_facts(path, &markers, facts);
        if has_suffix(file_name(path), ".py") && !is_incidental(path) {
            python_sources.push(path);
        }
    }
    if !python_manifest {
        python_source_facts(&python_sources, facts);
    }
    for (kind, what) in TREE_KINDS {
        let mut entry = look(
            kind,
            Source::Tree,
            facts.count(kind, Source::Tree),
            format!("{what} in {} files", paths.len()),
        );
        if truncated {
            entry.status = LookStatus::Partial;
            entry.note = Some("GitHub truncated the file list; facts are a lower bound".to_owned());
        }
        looks.push(entry);
    }
}

/// The build, lock and pin facts of one path; whether it is a non-incidental
/// Python manifest.
fn path_facts(path: &str, markers: &[(&str, &str, &str)], facts: &mut Facts) -> bool {
    let name = file_name(path);
    let incidental = is_incidental(path);
    let at = || path.to_owned();
    let mut python_manifest = false;
    for (marker, family, ecosystem) in markers {
        if name == *marker && !LOCKFILES.contains(marker) {
            facts.add(
                FactKind::BuildSystem,
                family,
                Source::Tree,
                incidental,
                Evidence {
                    at: at(),
                    text: format!("{ecosystem} marker"),
                },
            );
            python_manifest |= *family == "python" && !incidental;
        }
    }
    if path.contains(".xcodeproj/") && name == "project.pbxproj" {
        facts.add(
            FactKind::BuildSystem,
            "apple",
            Source::Tree,
            incidental,
            Evidence {
                at: at(),
                text: "xcode project".to_owned(),
            },
        );
    }
    if LOCKFILES.contains(&name) {
        facts.add(
            FactKind::Lockfile,
            name,
            Source::Tree,
            incidental,
            Evidence {
                at: at(),
                text: "lockfile".to_owned(),
            },
        );
    }
    let requirements = (name.starts_with("requirements") || name.starts_with("constraints"))
        && has_suffix(name, ".txt");
    if PIN_MANIFESTS.contains(&name) || requirements {
        facts.add(
            FactKind::PinManifest,
            if requirements {
                "requirements*.txt"
            } else {
                name
            },
            Source::Tree,
            incidental,
            Evidence {
                at: at(),
                text: "pin manifest".to_owned(),
            },
        );
    }
    python_manifest
}

/// Python sources with no manifest still make a Python project.
fn python_source_facts(sources: &[&String], facts: &mut Facts) {
    let count = sources.len();
    for path in sources.iter().take(EVIDENCE_CAP) {
        facts.add(
            FactKind::BuildSystem,
            "python",
            Source::Tree,
            false,
            Evidence {
                at: (*path).clone(),
                text: format!(
                    "python source ({count} outside incidental directories, no manifest)"
                ),
            },
        );
    }
    if let Some(fact) = facts
        .by_key
        .get_mut(&(FactKind::BuildSystem, "python".to_owned()))
    {
        fact.occurrences = fact.occurrences.max(count);
    }
}

pub(super) fn test_framework_facts(
    paths: Option<&Vec<String>>,
    facts: &mut Facts,
    looks: &mut Vec<Look>,
    snapshot: &Snapshot,
) {
    if let Some(paths) = paths {
        let cargo_roots: BTreeSet<&str> = paths
            .iter()
            .filter(|path| file_name(path) == "Cargo.toml")
            .map(|path| path.strip_suffix("Cargo.toml").unwrap_or_default())
            .collect();
        for path in paths {
            let name = file_name(path);
            let framework = if name == "conftest.py" || name == "pytest.ini" {
                Some("pytest")
            } else if has_suffix(name, "_test.go") {
                Some("go test")
            } else if name.starts_with("jest.config.") {
                Some("jest")
            } else if name.starts_with("vitest.config.") {
                Some("vitest")
            } else if has_suffix(name, ".rs")
                && cargo_roots.iter().any(|root| {
                    path.strip_prefix(root)
                        .is_some_and(|rest| rest.starts_with("tests/"))
                })
            {
                Some("cargo test")
            } else {
                None
            };
            if let Some(framework) = framework {
                facts.add(
                    FactKind::TestFramework,
                    framework,
                    Source::Tree,
                    is_incidental(path),
                    Evidence {
                        at: path.clone(),
                        text: "test file".to_owned(),
                    },
                );
            }
        }
    }
    let from_commands = [
        ("cargo test", "cargo test"),
        ("cargo nextest", "cargo test"),
        ("pytest", "pytest"),
        ("python -m unittest", "python unittest"),
        ("ctest", "ctest"),
        ("go test", "go test"),
        ("jest", "jest"),
        ("vitest", "vitest"),
        ("npm test", "npm test"),
    ];
    let commands: Vec<(String, Vec<Evidence>)> = facts
        .by_key
        .values()
        .filter(|fact| fact.kind == FactKind::CiCommand)
        .map(|fact| (fact.value.clone(), fact.evidence.clone()))
        .collect();
    for (command, evidence) in commands {
        if let Some((_, framework)) = from_commands.iter().find(|(name, _)| *name == command) {
            for row in evidence {
                facts.add(
                    FactKind::TestFramework,
                    framework,
                    Source::Workflow,
                    false,
                    row,
                );
            }
        }
    }
    let tree_error = snapshot.paths.as_ref().err();
    let workflow_error = snapshot.workflows.as_ref().err();
    let found = facts
        .by_key
        .keys()
        .filter(|(kind, _)| *kind == FactKind::TestFramework)
        .count();
    let searched = "test files (conftest.py, *_test.go, jest/vitest configs, Cargo tests/) and test commands in workflows".to_owned();
    let mut entry = look(FactKind::TestFramework, Source::Tree, found, searched);
    let errors: Vec<String> = [
        tree_error.map(|error| format!("file list: {error}")),
        workflow_error.map(|error| format!("workflows: {error}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !errors.is_empty() {
        entry.status = if errors.len() == 2 {
            LookStatus::Unavailable
        } else {
            LookStatus::Partial
        };
        entry.note = Some(errors.join("; "));
    }
    looks.push(entry);
}
