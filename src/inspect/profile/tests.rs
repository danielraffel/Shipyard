use std::collections::BTreeMap;
use std::io::Read as _;

use serde_json::Value;

use super::gather::{GatherOptions, gather};
use super::*;

fn answers(repo: &str) -> BTreeMap<String, Value> {
    let path = format!(
        "{}/tests/fixtures/inspect-profile/{}.json.gz",
        env!("CARGO_MANIFEST_DIR"),
        repo.replace('/', "__")
    );
    let file = std::fs::File::open(&path).unwrap_or_else(|error| panic!("{path}: {error}"));
    let mut text = String::new();
    flate2::read::GzDecoder::new(file)
        .read_to_string(&mut text)
        .expect("gunzip");
    serde_json::from_str(&text).expect("answers")
}

/// Answers `gh api` from captured answers; anything not captured is an error
/// naming the call, so a read the fixtures do not cover fails loudly.
struct Fake {
    answers: BTreeMap<String, Value>,
    overrides: BTreeMap<String, Result<String, String>>,
    calls: std::cell::RefCell<Vec<String>>,
}

impl Fake {
    fn new(repo: &str) -> Self {
        Self {
            answers: answers(repo),
            overrides: BTreeMap::new(),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn with(mut self, key: &str, answer: Result<&str, &str>) -> Self {
        self.overrides.insert(
            key.to_owned(),
            answer.map(str::to_owned).map_err(str::to_owned),
        );
        self
    }

    fn answer(&self, args: &[String]) -> Result<String, String> {
        let key = if args.get(1).map(String::as_str) == Some("graphql") {
            "graphql:workflows".to_owned()
        } else {
            args.get(1).cloned().unwrap_or_default()
        };
        self.calls.borrow_mut().push(key.clone());
        if let Some(answer) = self.overrides.get(&key) {
            return answer.clone();
        }
        match self.answers.get(&key) {
            Some(answer) => match (answer.get("ok"), answer.get("err")) {
                (Some(ok), _) => Ok(ok.to_string()),
                (_, Some(Value::String(err))) => Err(err.clone()),
                _ => Err(format!("malformed fixture for {key}")),
            },
            None => Err(format!("no captured answer for {key}")),
        }
    }
}

fn profile_of(fake: &Fake, repo: &str, max_runs: usize) -> ProjectProfile {
    let reader = |args: &[String]| fake.answer(args);
    let (snapshot, calls) = gather(
        &reader,
        &GatherOptions {
            repo,
            base: "main",
            max_runs,
        },
    );
    detect(repo, "main", &snapshot, calls, None)
}

fn labels(profile: &ProjectProfile) -> Vec<&str> {
    profile.labels.iter().map(|label| label.name).collect()
}

fn look_of(profile: &ProjectProfile, kind: FactKind, source: Source) -> &Look {
    profile
        .looks
        .iter()
        .find(|look| look.kind == kind && look.source == source)
        .unwrap_or_else(|| panic!("no look for {kind:?} in {source:?}"))
}

fn values(profile: &ProjectProfile, kind: FactKind) -> Vec<&str> {
    profile
        .facts_of(kind)
        .map(|fact| fact.value.as_str())
        .collect()
}

const SHIPYARD: &str = "danielraffel/Shipyard";
const TARTCI: &str = "danielraffel/tartci";
const VELLUM: &str = "Generous-Corp/vellum";
const PULP: &str = "Generous-Corp/pulp";

/// Each of the four repositories reads as what it is: a Rust CLI with no
/// branch protection, a Python ops tool whose one required check is `lint`,
/// and two `CMake` desktop projects with a web package behind a merge queue.
#[test]
fn the_four_repositories_read_as_what_they_are() {
    let shipyard = profile_of(&Fake::new(SHIPYARD), SHIPYARD, 8);
    assert_eq!(labels(&shipyard), ["rust-cli"], "{}", render(&shipyard));
    assert!(shipyard.has(FactKind::BuildSystem, "rust"));
    assert!(shipyard.has(FactKind::Lockfile, "Cargo.lock"));
    assert!(shipyard.has(FactKind::PinManifest, "rust-toolchain.toml"));
    assert!(shipyard.has(FactKind::CiCommand, "cargo test"));
    assert!(shipyard.has(FactKind::ReleaseWorkflow, ".github/workflows/release.yml"));
    assert!(shipyard.has(
        FactKind::RunnerClass,
        "self-hosted [self-hosted, local-mac]"
    ));
    let queue = look_of(&shipyard, FactKind::MergeQueue, Source::Settings);
    assert_eq!(queue.status, LookStatus::NoneFound);
    let required = look_of(&shipyard, FactKind::RequiredCheck, Source::Settings);
    assert_eq!(required.status, LookStatus::NoneFound);
    assert!(
        required
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("no branch protection")
    );

    let tartci = profile_of(&Fake::new(TARTCI), TARTCI, 8);
    assert_eq!(labels(&tartci), ["python-tool"], "{}", render(&tartci));
    assert_eq!(values(&tartci, FactKind::RequiredCheck), ["lint"]);
    assert!(tartci.has(FactKind::CiCommand, "python -m unittest"));
    assert_eq!(
        look_of(&tartci, FactKind::Lockfile, Source::Tree).status,
        LookStatus::NoneFound
    );
    assert_eq!(
        look_of(&tartci, FactKind::MergeQueue, Source::Settings).status,
        LookStatus::NoneFound
    );
    assert_eq!(tartci.api_calls, 5, "no computed runs-on, so no run reads");

    let vellum = profile_of(&Fake::new(VELLUM), VELLUM, 8);
    assert_eq!(
        labels(&vellum),
        ["desktop-native", "web"],
        "{}",
        render(&vellum)
    );
    assert!(vellum.has(FactKind::MergeQueue, "present"));
    assert_eq!(
        values(&vellum, FactKind::RequiredCheck),
        [
            "clean-release",
            "forbidden-deps",
            "gpu-macos-arm64",
            "product-quality",
            "provenance-verify",
            "sterile-consumer"
        ]
    );

    let pulp = profile_of(&Fake::new(PULP), PULP, 8);
    assert_eq!(
        labels(&pulp),
        ["desktop-native", "web"],
        "{}",
        render(&pulp)
    );
    let queue = pulp.facts_of(FactKind::MergeQueue).next().expect("queue");
    assert!(
        queue.evidence[0].text.contains("grouping ALLGREEN"),
        "{queue:?}"
    );
    assert!(pulp.has(FactKind::RequiredCheck, "macos"));
    // The required `macos` gate's runner is routed by a variable, so only a
    // run shows it: build.yml is read first because it produces that check.
    let observed: Vec<&Fact> = pulp
        .facts_of(FactKind::RunnerClass)
        .filter(|fact| fact.source == Source::Runs && fact.value.contains("pulp-build-vm"))
        .collect();
    assert!(!observed.is_empty(), "{}", render(&pulp));
    assert!(
        observed[0].value.starts_with("self-hosted ["),
        "{observed:?}"
    );
    assert!(observed[0].evidence[0].at.contains("(build.yml)"));
    // Rust lives only in examples, fixtures and experimental code.
    let rust = pulp
        .facts_of(FactKind::BuildSystem)
        .find(|fact| fact.value == "rust")
        .expect("rust fact");
    assert!(rust.incidental);
    assert!(!pulp.has(FactKind::BuildSystem, "rust"));
    assert_eq!(
        look_of(&pulp, FactKind::RunnerClass, Source::Runs).status,
        LookStatus::Partial
    );
    assert_eq!(pulp.api_calls, 5 + 2 * 8);
}

/// A workflow fact's `path:line` names the very line it quotes.
#[test]
fn every_workflow_evidence_line_is_the_line_it_names() {
    let mut checked = 0;
    for repo in [SHIPYARD, TARTCI, VELLUM, PULP] {
        let fake = Fake::new(repo);
        let texts: BTreeMap<String, String> = fake.answers["graphql:workflows"]["ok"]
            .pointer("/data/repository/object/entries")
            .and_then(Value::as_array)
            .expect("entries")
            .iter()
            .filter_map(|entry| {
                Some((
                    format!(".github/workflows/{}", entry["name"].as_str()?),
                    entry.pointer("/object/text")?.as_str()?.to_owned(),
                ))
            })
            .collect();
        let profile = profile_of(&fake, repo, 8);
        for fact in profile
            .facts
            .iter()
            .filter(|fact| fact.source == Source::Workflow)
        {
            for row in &fact.evidence {
                let Some((path, line)) = row.at.rsplit_once(':') else {
                    continue;
                };
                let Ok(line) = line.parse::<usize>() else {
                    continue;
                };
                let text = texts[path]
                    .lines()
                    .nth(line - 1)
                    .expect("line exists")
                    .trim();
                if fact.kind == FactKind::RunnerClass {
                    // A block `runs-on:` is joined onto one line, so the
                    // line's own value is a prefix of the quoted one.
                    let quoted = row
                        .text
                        .split_once("runs-on:")
                        .expect("quoted runs-on")
                        .1
                        .trim();
                    let own = text
                        .strip_prefix("runs-on:")
                        .expect("a runs-on line")
                        .trim();
                    let own = if [">-", ">", "|", "|-"].contains(&own) {
                        ""
                    } else {
                        own
                    };
                    assert!(
                        quoted.starts_with(own),
                        "{repo} {}: `{quoted}` vs `{text}`",
                        row.at
                    );
                } else {
                    assert!(
                        text.starts_with(row.text.as_str()),
                        "{repo} {}: `{}` vs `{text}`",
                        row.at,
                        row.text
                    );
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 100, "only {checked} evidence rows checked");
}

/// A source that could not be read leaves its kinds unavailable, with the
/// error, and adds no fact: it is never read as "none".
#[test]
fn an_unreadable_source_is_never_reported_as_absent() {
    let rules = format!("repos/{PULP}/rules/branches/main");
    let protection = format!("repos/{PULP}/branches/main/protection/required_status_checks");
    let fake = Fake::new(PULP)
        .with(
            &rules,
            Err("gh: Resource not accessible by integration (HTTP 403)"),
        )
        .with(
            &protection,
            Err("gh: Resource not accessible by integration (HTTP 403)"),
        );
    let profile = profile_of(&fake, PULP, 0);
    for kind in [FactKind::MergeQueue, FactKind::RequiredCheck] {
        let look = look_of(&profile, kind, Source::Settings);
        assert_eq!(look.status, LookStatus::Unavailable, "{look:?}");
        assert!(
            look.note
                .as_deref()
                .unwrap_or_default()
                .contains("HTTP 403")
        );
        assert_eq!(profile.facts_of(kind).count(), 0);
    }
    // A 404 on protection alone means "unprotected", never "unreadable".
    let unprotected = Fake::new(PULP).with(&protection, Err("gh: Branch not protected (HTTP 404)"));
    let profile = profile_of(&unprotected, PULP, 0);
    assert_eq!(
        look_of(&profile, FactKind::RequiredCheck, Source::Settings).status,
        LookStatus::NoneFound
    );

    let commit = Fake::new(TARTCI).answers[&format!("repos/{TARTCI}/commits/main")]["ok"]["sha"]
        .as_str()
        .expect("sha")
        .to_owned();
    let tree = format!("repos/{TARTCI}/git/trees/{commit}?recursive=1");
    for (answer, expected) in [
        (Err("gh: HTTP 502"), "HTTP 502"),
        (
            Ok(r#"{"sha":"x","truncated":false,"tree":[]}"#),
            "returned no files",
        ),
    ] {
        let fake = Fake::new(TARTCI).with(&tree, answer);
        let profile = profile_of(&fake, TARTCI, 8);
        for kind in [
            FactKind::BuildSystem,
            FactKind::Lockfile,
            FactKind::PinManifest,
        ] {
            let look = look_of(&profile, kind, Source::Tree);
            assert_eq!(look.status, LookStatus::Unavailable, "{look:?}");
            assert!(
                look.note.as_deref().unwrap_or_default().contains(expected),
                "{look:?}"
            );
            assert_eq!(profile.facts_of(kind).count(), 0);
        }
    }

    // The control between the file list and the text read: a workflow the
    // list names but the text read lost makes the workflow kinds unavailable.
    let mut short = Fake::new(SHIPYARD).answers["graphql:workflows"]["ok"].clone();
    short
        .pointer_mut("/data/repository/object/entries")
        .and_then(Value::as_array_mut)
        .expect("entries")
        .pop();
    let short = short.to_string();
    let fake = Fake::new(SHIPYARD).with("graphql:workflows", Ok(&short));
    let profile = profile_of(&fake, SHIPYARD, 8);
    for kind in [
        FactKind::CiCommand,
        FactKind::RunnerClass,
        FactKind::ReleaseWorkflow,
    ] {
        let look = look_of(&profile, kind, Source::Workflow);
        assert_eq!(look.status, LookStatus::Unavailable, "{look:?}");
        assert!(
            look.note
                .as_deref()
                .unwrap_or_default()
                .contains("holds 9 workflow file(s)")
        );
    }
}

fn synthetic(paths: &[&str], workflow: &str) -> Snapshot {
    Snapshot {
        commit: Ok("0".repeat(40)),
        paths: Ok((paths.iter().map(|path| (*path).to_owned()).collect(), false)),
        workflows: Ok(vec![WorkflowFile {
            path: ".github/workflows/ci.yml".to_owned(),
            text: Some(workflow.to_owned()),
            note: None,
        }]),
        rules: Ok(Value::Array(Vec::new())),
        protection: Ok(None),
        runs: RunSample::default(),
    }
}

/// A fixture's package.json is a fact, marked incidental, and makes no label;
/// the same file in a package directory does.
#[test]
fn incidental_files_are_kept_but_make_no_label() {
    let workflow =
        "on: [push]\njobs:\n  web:\n    runs-on: ubuntu-latest\n    steps:\n      - run: npm ci\n";
    let fixture = detect(
        "o/r",
        "main",
        &synthetic(&["fixtures/app/package.json", "README.md"], workflow),
        0,
        None,
    );
    let node = fixture
        .facts_of(FactKind::BuildSystem)
        .find(|fact| fact.value == "node")
        .expect("node");
    assert!(node.incidental);
    assert!(labels(&fixture).is_empty(), "{}", render(&fixture));
    let package = detect(
        "o/r",
        "main",
        &synthetic(&["packages/app/package.json", "README.md"], workflow),
        0,
        None,
    );
    assert_eq!(labels(&package), ["web"]);
}

/// The run budget goes to the workflows that produce a required check first,
/// and every workflow it could not read is named.
#[test]
fn the_run_budget_reads_required_producers_first_and_names_the_rest() {
    let fake = Fake::new(PULP);
    let reader = |args: &[String]| fake.answer(args);
    let (snapshot, calls) = gather(
        &reader,
        &GatherOptions {
            repo: PULP,
            base: "main",
            max_runs: 1,
        },
    );
    let sampled: Vec<&str> = snapshot
        .runs
        .sampled
        .iter()
        .map(|run| run.workflow.as_str())
        .collect();
    assert_eq!(sampled, ["build.yml"]);
    assert_eq!(
        snapshot.runs.over_budget.len(),
        snapshot.runs.wanted.len() - 1
    );
    assert!(
        snapshot
            .runs
            .over_budget
            .contains(&"coverage.yml".to_owned())
    );
    assert_eq!(calls, 5 + 2);
    let profile = detect(PULP, "main", &snapshot, calls, None);
    let look = look_of(&profile, FactKind::RunnerClass, Source::Runs);
    assert_eq!(look.status, LookStatus::Partial);
    assert!(
        look.note
            .as_deref()
            .unwrap_or_default()
            .contains("coverage.yml")
    );
}

#[test]
fn a_group_and_labels_runs_on_is_read_as_its_labels() {
    let expr = "group: pulp-trusted-build labels: - self-hosted - macOS - pulp-build-vm";
    assert_eq!(
        static_runner_class(expr),
        "self-hosted [self-hosted, macOS, pulp-build-vm] (group pulp-trusted-build)"
    );
    assert!(!runs_on_is_computed(expr));
    assert!(runs_on_is_computed("${{ fromJSON(vars.X) }}"));
    assert_eq!(static_runner_class("macos-15"), "github-hosted [macos-15]");
}

/// A computed `runs-on:` says nothing about the platform until a run shows
/// where it landed, so it never makes a `CMake` project desktop-native.
#[test]
fn a_computed_runner_is_not_a_platform_until_a_run_shows_it() {
    let workflow = "on: [push]\njobs:\n  build-macos:\n    runs-on: ${{ fromJSON(vars.MACOS_RUNS_ON_JSON) }}\n    steps:\n      - run: cmake -S . -B build\n";
    let mut snapshot = synthetic(&["CMakeLists.txt"], workflow);
    let computed = detect("o/r", "main", &snapshot, 0, None);
    // Control: the computed runner is read, and names macOS in its variable.
    assert!(
        computed.has(FactKind::RunnerClass, "routed by vars.MACOS_RUNS_ON_JSON"),
        "{}",
        render(&computed)
    );
    assert!(labels(&computed).is_empty(), "{}", render(&computed));
    snapshot.runs.sampled.push(SampledRun {
        workflow: "ci.yml".to_owned(),
        run_id: 7,
        jobs: vec![serde_json::json!({
            "name": "build-macos",
            "labels": ["self-hosted", "macOS"],
            "runner_name": "mac-1",
            "runner_group_name": "Default",
        })],
        partial: false,
    });
    let observed = detect("o/r", "main", &snapshot, 0, None);
    assert_eq!(
        labels(&observed),
        ["desktop-native"],
        "{}",
        render(&observed)
    );
}
