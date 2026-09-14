//! Fixtures for the `on:` reader.
//!
//! Every "real" fixture under `tests/fixtures/triggers/` is the verbatim `on:`
//! block of a workflow that was live on `origin/main` when this was written —
//! the five Pulp producers of Pulp's five required contexts, plus spectr's
//! single gate. They are captures, not hand-written examples, because a reader
//! whose corpus is its own author's idea of YAML is a reader that agrees with
//! itself.
//!
//! The must-refuse half matters more than the must-parse half. A mis-read
//! filter that admits is a false pass, so each refusing form asserts the
//! boundary *and* that no filter list came back.

use super::*;

const PULP_BUILD: &str = include_str!("../../../tests/fixtures/triggers/pulp-build.yml");
const PULP_VERSION_SKILL: &str =
    include_str!("../../../tests/fixtures/triggers/pulp-version-skill-check.yml");
const PULP_WCLAP: &str = include_str!("../../../tests/fixtures/triggers/pulp-wclap-cloudflare.yml");
const PULP_VELLUM_FREEZE: &str =
    include_str!("../../../tests/fixtures/triggers/pulp-vellum-freeze-check.yml");
const PULP_VELLUM_TRUSTED: &str =
    include_str!("../../../tests/fixtures/triggers/pulp-vellum-trusted-gate.yml");
const SPECTR_GATE: &str =
    include_str!("../../../tests/fixtures/triggers/spectr-m5-product-acceptance.yml");

/// The six real captures, for the corpus-wide assertions.
const REAL_FIXTURES: &[(&str, &str)] = &[
    ("pulp/build.yml", PULP_BUILD),
    ("pulp/version-skill-check.yml", PULP_VERSION_SKILL),
    ("pulp/wclap-cloudflare.yml", PULP_WCLAP),
    ("pulp/vellum-freeze-check.yml", PULP_VELLUM_FREEZE),
    ("pulp/vellum-trusted-gate.yml", PULP_VELLUM_TRUSTED),
    ("spectr/m5-product-acceptance.yml", SPECTR_GATE),
];

#[test]
fn every_real_capture_parses_exactly() {
    for (name, source) in REAL_FIXTURES {
        let parsed = parse_workflow_triggers(source)
            .unwrap_or_else(|error| panic!("{name} refused: {error}"));
        assert!(
            parsed.has_pull_request_shaped_event(),
            "{name} declares no pull-request-shaped event"
        );
    }
}

// ---------------------------------------------------------------------------
// T1 — base exclusion, on the capture that caused the incident
// ---------------------------------------------------------------------------

#[test]
fn spectr_gate_excludes_the_stacked_base_and_names_the_clause() {
    let triggers = parse_workflow_triggers(SPECTR_GATE).expect("spectr gate parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    let verdict = filter.admits_base("fix/help-overlay-layout-and-scroll");
    match &verdict {
        Admit::No { clause, detail } => {
            assert_eq!(clause, "branches: [main]");
            assert!(
                detail.contains("fix/help-overlay-layout-and-scroll"),
                "{detail}"
            );
        }
        other => panic!("expected exclusion, got {other:?}"),
    }
    assert!(verdict.excludes());
}

#[test]
fn spectr_gate_admits_main_as_its_control() {
    let triggers = parse_workflow_triggers(SPECTR_GATE).expect("spectr gate parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    assert_eq!(filter.admits_base("main"), Admit::Yes);
}

#[test]
fn pulp_build_admits_develop_glob_but_not_an_arbitrary_branch() {
    let triggers = parse_workflow_triggers(PULP_BUILD).expect("build.yml parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    assert_eq!(filter.admits_base("main"), Admit::Yes);
    assert_eq!(filter.admits_base("develop/audio"), Admit::Yes);
    assert!(filter.admits_base("feature/x").excludes());
}

// ---------------------------------------------------------------------------
// T2 — path exclusion, both directions
// ---------------------------------------------------------------------------

#[test]
fn spectr_gate_excludes_a_docs_only_diff() {
    let triggers = parse_workflow_triggers(SPECTR_GATE).expect("spectr gate parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    let changed = vec!["docs/x.md".to_owned(), "planning/y.md".to_owned()];
    match filter.admits_paths(&changed) {
        Admit::No { clause, detail } => {
            assert!(clause.starts_with("paths-ignore:"), "{clause}");
            assert!(detail.contains("every one of the 2"), "{detail}");
        }
        other => panic!("expected exclusion, got {other:?}"),
    }
}

#[test]
fn spectr_gate_admits_a_source_diff_as_its_control() {
    let triggers = parse_workflow_triggers(SPECTR_GATE).expect("spectr gate parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    assert_eq!(
        filter.admits_paths(&["src/a.cpp".to_owned()]),
        Admit::Yes,
        "a source-only diff must survive paths-ignore"
    );
}

#[test]
fn a_mixed_diff_is_admitted_because_one_file_escapes_the_ignore() {
    let triggers = parse_workflow_triggers(SPECTR_GATE).expect("spectr gate parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    let changed = vec!["docs/x.md".to_owned(), "src/a.cpp".to_owned()];
    assert_eq!(filter.admits_paths(&changed), Admit::Yes);
}

#[test]
fn a_path_filter_refuses_past_githubs_evaluation_limit() {
    let triggers = parse_workflow_triggers(SPECTR_GATE).expect("spectr gate parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    let changed: Vec<String> = (0..=PATHS_DIFF_LIMIT)
        .map(|n| format!("docs/{n}.md"))
        .collect();
    match filter.admits_paths(&changed) {
        Admit::Unknown(unknown) => assert_eq!(unknown.boundary, "diff_size"),
        other => panic!("expected Unknown past the limit, got {other:?}"),
    }
}

#[test]
fn an_unfiltered_event_admits_every_diff_including_an_unreadable_one() {
    let triggers = parse_workflow_triggers(PULP_VELLUM_FREEZE).expect("parses");
    let filter = triggers.pull_request.expect("pull_request declared");
    assert_eq!(filter.admits_paths(&[]), Admit::Yes);
    assert_eq!(filter.admits_base("anything/at/all"), Admit::Yes);
}

// ---------------------------------------------------------------------------
// T7 — the `edited` warning, on real files that differ
// ---------------------------------------------------------------------------

#[test]
fn the_retarget_refire_property_separates_the_real_producers() {
    let refires = |source| {
        parse_workflow_triggers(source)
            .expect("parses")
            .refires_on_retarget()
    };
    // Declares `edited`.
    assert!(refires(PULP_VERSION_SKILL));
    assert!(refires(PULP_VELLUM_TRUSTED));
    // Does not — a retarget will not re-fire these.
    assert!(!refires(PULP_BUILD));
    assert!(!refires(PULP_VELLUM_FREEZE));
    assert!(!refires(SPECTR_GATE));
}

#[test]
fn default_activity_types_apply_when_none_are_declared() {
    let filter = EventFilter::default();
    assert!(filter.has_type("opened"));
    assert!(filter.has_type("synchronize"));
    assert!(filter.has_type("reopened"));
    assert!(!filter.has_type("edited"));
}

// ---------------------------------------------------------------------------
// T6 — the spellings, and the refusals
// ---------------------------------------------------------------------------

#[test]
fn scalar_flow_and_mapping_spellings_all_read() {
    let scalar = parse_workflow_triggers("on: pull_request\njobs:\n").expect("scalar");
    assert!(scalar.pull_request.is_some());

    let flow = parse_workflow_triggers("on: [push, pull_request]\njobs:\n").expect("flow");
    assert!(flow.pull_request.is_some());
    assert!(flow.push);

    let mapping =
        parse_workflow_triggers("on:\n  pull_request:\n    branches: [main]\n").expect("map");
    assert_eq!(
        mapping.pull_request.expect("declared").branches,
        Some(vec!["main".to_owned()])
    );

    let quoted = parse_workflow_triggers("\"on\":\n  pull_request:\n").expect("quoted");
    assert!(quoted.pull_request.is_some());
    assert!(!quoted.yaml_true_key);

    let yaml_true = parse_workflow_triggers("true:\n  pull_request:\n").expect("true key");
    assert!(yaml_true.pull_request.is_some());
    assert!(
        yaml_true.yaml_true_key,
        "a bare `on` folded to `true:` must be reported, not silently accepted"
    );
}

#[test]
fn block_list_and_flow_list_filters_read_identically() {
    let block = parse_workflow_triggers(
        "on:\n  pull_request:\n    branches:\n      - main\n      - 'develop/**'\n",
    )
    .expect("block list");
    let flow =
        parse_workflow_triggers("on:\n  pull_request:\n    branches: [main, 'develop/**']\n")
            .expect("flow list");
    assert_eq!(block.pull_request, flow.pull_request);
}

#[test]
fn a_workflow_with_no_on_block_is_a_fact_not_an_unknown() {
    let triggers = parse_workflow_triggers("name: x\njobs:\n  a:\n    runs-on: ubuntu-latest\n")
        .expect("no on: block is not a parse failure");
    assert!(!triggers.has_pull_request_shaped_event());
    assert!(triggers.events.is_empty());
}

/// Each of these must refuse, and the assertion checks the boundary so a
/// refusal for the wrong reason is not mistaken for coverage.
#[test]
fn every_must_refuse_form_refuses_with_its_boundary() {
    let cases: &[(&str, &str)] = &[
        (
            "expression",
            "on:\n  pull_request:\n    branches: [\"${{ vars.BASE }}\"]\n",
        ),
        (
            "anchor",
            "on:\n  pull_request:\n    branches: &b\n      - main\n",
        ),
        ("tab", "on:\n\tpull_request:\n"),
        ("duplicate", "on:\n  push:\non:\n  pull_request:\n"),
        (
            "multi_document",
            "name: a\njobs: {}\n---\non:\n  pull_request:\n",
        ),
        (
            "activity_type",
            "on:\n  pull_request:\n    types: [opened, retargeted]\n",
        ),
        (
            "pattern",
            "on:\n  pull_request:\n    branches: ['^main$']\n",
        ),
        (
            "conflicting_filters",
            "on:\n  pull_request:\n    branches: [main]\n    branches-ignore: [x]\n",
        ),
        (
            "negated_ignore",
            "on:\n  pull_request:\n    paths-ignore: ['!src/**']\n",
        ),
        ("shape", "on:\n  pull_request:\n    branches: main\n"),
        (
            "shape",
            "on:\n  pull_request:\n    unknown-filter: [main]\n",
        ),
        (
            "duplicate",
            "on:\n  pull_request:\n    branches: [main]\n    branches: [dev]\n",
        ),
    ];
    for (boundary, source) in cases {
        let error = parse_workflow_triggers(source)
            .expect_err(&format!("must refuse [{boundary}]:\n{source}"));
        assert_eq!(
            &error.boundary, boundary,
            "wrong boundary for:\n{source}\ngot: {error}"
        );
    }
}

#[test]
fn a_refused_block_yields_no_filter_list_at_all() {
    // The false-pass shape this reader exists to prevent: a partially-read
    // filter that admits what the real filter excludes.
    let error = parse_workflow_triggers(
        "on:\n  pull_request:\n    branches: [main]\n    paths: [\"${{ vars.P }}\"]\n",
    )
    .expect_err("an expression inside the block must refuse");
    assert_eq!(error.boundary, "expression");
    assert!(error.line.is_some(), "a refusal must name its line");
}

#[test]
fn an_indented_triple_dash_is_content_not_a_document_separator() {
    // Real capture: Pulp's `release-cli.yml` carries a markdown horizontal
    // rule inside a release-body block scalar at line 1904. Trimming before
    // the column-0 test made the reader refuse that file outright — found by
    // the whole-directory control, which is the only thing that would have.
    let source = "on:\n  pull_request:\n    branches: [main]\njobs:\n  a:\n    steps:\n                        - run: |\n          notes\n\n          ---\n\n          ## Install\n";
    let triggers = parse_workflow_triggers(source).expect("an indented --- is not a separator");
    assert!(triggers.pull_request.is_some());

    // Control: at column 0 it still refuses.
    let error = parse_workflow_triggers("name: a\njobs: {}\n---\non:\n  pull_request:\n")
        .expect_err("a column-0 --- after content is a second document");
    assert_eq!(error.boundary, "multi_document");
}

#[test]
fn a_comment_containing_an_expression_does_not_refuse() {
    // Refusing on a comment would make the reader unusable on real files:
    // Pulp's own build.yml documents `${{ fromJSON(...) }}` in prose.
    let triggers = parse_workflow_triggers(
        "on:\n  # routed through ${{ vars.X }} elsewhere\n  pull_request:\n    branches: [main]\n",
    )
    .expect("a comment is not a value");
    assert!(triggers.pull_request.is_some());
}

// ---------------------------------------------------------------------------
// The glob matcher, against GitHub's own documented examples
// ---------------------------------------------------------------------------

#[test]
fn branch_patterns_match_githubs_documented_examples() {
    let cases: &[(&str, &str, bool)] = &[
        ("feature/*", "feature/my-branch", true),
        ("feature/*", "feature/your-branch", true),
        ("feature/*", "feature/beta-a/my-branch", false),
        ("feature/**", "feature/beta-a/my-branch", true),
        ("feature/**", "feature/mona/the/octocat", true),
        ("main", "main", true),
        ("main", "mains", false),
        ("releases/**-alpha", "releases/beta/3-alpha", true),
        ("v2*", "v2", true),
        ("v2*", "v2.0", true),
        ("v[12].[0-9]", "v1.0", true),
        ("v[12].[0-9]", "v3.0", false),
        ("[CB]at", "Cat", true),
        ("[CB]at", "Bat", true),
        ("[CB]at", "Hat", false),
        ("[1-2]00", "100", true),
        ("[1-2]00", "200", true),
        ("[1-2]00", "300", false),
        ("Octo*", "Octocat", true),
        ("refs/heads/main", "main", true),
        ("mona/octocat", "mona/octocat", true),
        ("*", "main", true),
        ("*", "feature/x", false),
        ("**", "feature/x", true),
    ];
    for (pattern, candidate, expected) in cases {
        let actual = matches_pattern(
            pattern.strip_prefix("refs/heads/").unwrap_or(pattern),
            candidate,
        )
        .unwrap_or_else(|error| panic!("{pattern} vs {candidate}: {error}"));
        assert_eq!(actual, *expected, "pattern `{pattern}` vs `{candidate}`");
    }
}

#[test]
fn path_patterns_match_githubs_documented_examples() {
    let cases: &[(&str, &str, bool)] = &[
        ("*.js", "app.js", true),
        ("*.js", "src/app.js", false),
        ("**.js", "src/app.js", true),
        ("**/*.md", "docs/a.md", true),
        ("**/*.md", "a.md", false),
        ("docs/**", "docs/a/b.md", true),
        ("docs/*", "docs/a/b.md", false),
        // `?` is a quantifier on the PRECEDING character, not a single-character
        // wildcard. GitHub's own example is `config?.json` matching
        // `config.json` and `confi.json`. A reader that implemented the
        // traditional glob meaning would admit `config1.json`, which GitHub
        // excludes — the exact direction of error this module refuses to make.
        ("config?.json", "config.json", true),
        ("config?.json", "confi.json", true),
        ("config?.json", "config1.json", false),
        ("v1+.0", "v1.0", true),
        ("v1+.0", "v111.0", true),
        ("v1+.0", "v.0", false),
        ("**/migrate-*.sql", "db/migrate-v1.sql", true),
    ];
    for (pattern, candidate, expected) in cases {
        let actual = matches_pattern(pattern, candidate)
            .unwrap_or_else(|error| panic!("{pattern} vs {candidate}: {error}"));
        assert_eq!(actual, *expected, "pattern `{pattern}` vs `{candidate}`");
    }
}

#[test]
fn negation_uses_last_match_wins_and_an_all_negative_list_matches_nothing() {
    let patterns = vec!["releases/**".to_owned(), "!releases/**-alpha".to_owned()];
    assert!(matches_any(&patterns, "releases/1", MatchKind::Branch).expect("ok"));
    assert!(!matches_any(&patterns, "releases/1-alpha", MatchKind::Branch).expect("ok"));

    let reordered = vec!["!releases/**-alpha".to_owned(), "releases/**".to_owned()];
    assert!(
        matches_any(&reordered, "releases/1-alpha", MatchKind::Branch).expect("ok"),
        "a positive pattern after a negative one wins; order is the semantics"
    );

    let only_negative = vec!["!main".to_owned()];
    assert!(!matches_any(&only_negative, "dev", MatchKind::Branch).expect("ok"));
}

#[test]
fn a_star_never_crosses_a_slash_but_a_double_star_does() {
    assert!(!matches_pattern("*", "a/b").expect("ok"));
    assert!(matches_pattern("**", "a/b").expect("ok"));
    assert!(matches_pattern("a/*/c", "a/b/c").expect("ok"));
    assert!(!matches_pattern("a/*/c", "a/b/x/c").expect("ok"));
    assert!(matches_pattern("a/**/c", "a/b/x/c").expect("ok"));
}
