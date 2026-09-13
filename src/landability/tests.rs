//! Fixtures for the landability classifier.
//!
//! Every test here is a **negative** control in the sense the repository's
//! standards mean it: each asserts the detector *fires*, on data captured from
//! the incident it was built for, so that a change which silences the detector
//! fails here rather than in production six hours into an outage.

use chrono::{TimeZone, Utc};

use super::assess::{AssessInput, assess};
use super::attestation::{
    AttestationSet, Generation, HostAttestation, JitLane, LaneCoverage, PersistentRunner,
};
use super::workflow::{
    RunsOnResolution, parse_workflow_jobs, resolve_runs_on_expr, transitive_needs,
};
use super::{Schedulability, fold_attestation};
use crate::fleet_service::{
    Boundary, LaneServiceThresholds, RegisteredRunner, RunnerScope, assess_lane_service,
};

/// The shape of `build.yml` on 2026-09-13, reduced to the routing facts.
///
/// Four jobs gate one required context, which is the entire point: `macos`'s
/// own job routed through a variable that was fine, and the two preamble jobs
/// it needs routed through the one that was not.
const BUILD_YML: &str = r#"
name: Build and Test

on:
  pull_request:

concurrency:
  group: build-${{ github.ref }}

jobs:
  resolve-provider:
    if: ${{ !inputs.local_proof }}
    # Routing preamble for the required gate.
    runs-on: ${{ fromJSON(vars.PULP_PREAMBLE_RUNS_ON_JSON || '"ubuntu-latest"') }}
    outputs:
      macos_runs_on_json: ${{ steps.resolve.outputs.macos_runs_on_json }}
    steps:
      - run: echo "${{ vars.PULP_LOCAL_MACOS_RUNS_ON_JSON }} ${{ vars.PULP_OVERFLOW_BUILD_MACOS_RUNS_ON_JSON }}"

  classify:
    if: ${{ !inputs.local_proof }}
    runs-on: ${{ fromJSON(vars.PULP_PREAMBLE_RUNS_ON_JSON || '"ubuntu-latest"') }}
    steps:
      - run: echo classify

  build:
    needs: [resolve-provider, classify]
    strategy:
      matrix:
        include: ${{ fromJSON(needs.resolve-provider.outputs.matrix_json) }}
    runs-on: ${{ fromJSON(matrix.runs_on_json) }}
    name: ${{ matrix.key == 'macos' && 'macos' || format('{0}', matrix.name) }}
    steps:
      - run: echo build

  macos:
    name: ${{ github.event_name == 'pull_request' && 'macos' || 'macos-pr-unused' }}
    needs: [resolve-provider, classify]
    runs-on: ${{ fromJSON(vars.PULP_ALIAS_RUNS_ON_JSON || '"ubuntu-latest"') }}
    steps:
      - run: echo alias

  windows-msvc-release-gate:
    needs: classify
    runs-on: windows-latest
    name: Windows MSVC release-path gate
    steps:
      - run: echo win
"#;

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 13, 5, 36, 0).unwrap()
}

fn fresh_attestation(host: &str, jit_labels: &[&str]) -> HostAttestation {
    HostAttestation {
        schema: 1,
        host: host.to_owned(),
        written_at: Some(now() - chrono::Duration::seconds(30)),
        interval_secs: Some(300),
        generation: Generation {
            writer: Some("test".to_owned()),
            ..Generation::default()
        },
        launchd_readable: Some(true),
        profile_readable: Some(true),
        profile_detail: None,
        persistent_runners: Vec::new(),
        jit_lanes: vec![JitLane {
            id: "pulp-gate".to_owned(),
            repo: "Generous-Corp/pulp".to_owned(),
            labels: jit_labels.iter().map(|s| (*s).to_owned()).collect(),
            supervisors: 2,
            fresh: 1,
            heartbeat_age_secs: Some(4),
            verdict: "attested".to_owned(),
            reason: "1 supervisor heartbeat within 300s".to_owned(),
        }],
    }
}

fn incident_variables() -> Vec<(String, String)> {
    vec![
        (
            "PULP_PREAMBLE_RUNS_ON_JSON".to_owned(),
            r#"["self-hosted","macOS","ARM64","pulp-preamble"]"#.to_owned(),
        ),
        (
            "PULP_ALIAS_RUNS_ON_JSON".to_owned(),
            r#"["self-hosted","macOS","ARM64","pulp-preamble"]"#.to_owned(),
        ),
        (
            "PULP_LOCAL_MACOS_RUNS_ON_JSON".to_owned(),
            r#"["self-hosted","macOS","ARM64","pulp-build","pulp-build-vm"]"#.to_owned(),
        ),
        (
            "PULP_OVERFLOW_BUILD_MACOS_RUNS_ON_JSON".to_owned(),
            "local-only".to_owned(),
        ),
    ]
}

/// The live census at 05:36 UTC: one online runner, carrying the gate labels
/// and **not** `pulp-preamble`.
fn incident_census() -> Vec<RegisteredRunner> {
    vec![RegisteredRunner {
        name: "studio-pulp-gate-01-97565-1".to_owned(),
        scope: RunnerScope::Repo,
        online: true,
        busy: true,
        labels: vec![
            "self-hosted".to_owned(),
            "macOS".to_owned(),
            "ARM64".to_owned(),
            "pulp-build".to_owned(),
            "pulp-build-vm".to_owned(),
        ],
    }]
}

// ---------------------------------------------------------------------------
// E1 — the headline. The 2026-09-13 state must refuse, naming the lane.
// ---------------------------------------------------------------------------

#[test]
fn e1_incident_state_refuses_and_names_the_preamble_lane() {
    let jobs = parse_workflow_jobs(BUILD_YML);
    let attestations = AttestationSet {
        hosts: vec![
            fresh_attestation(
                "m5",
                &[
                    "self-hosted",
                    "macOS",
                    "ARM64",
                    "pulp-build",
                    "pulp-build-vm",
                ],
            ),
            fresh_attestation(
                "m3",
                &[
                    "self-hosted",
                    "macOS",
                    "ARM64",
                    "pulp-build",
                    "pulp-build-vm",
                ],
            ),
        ],
        unreadable: Vec::new(),
    };
    let contexts = vec!["macos".to_owned()];
    let variables = incident_variables();
    let census = incident_census();
    let input = AssessInput {
        contexts: &contexts,
        contexts_source: "branch_protection",
        jobs: &jobs,
        variables: &variables,
        census: &census,
        census_boundary: None,
        attestations: &attestations,
        thresholds: LaneServiceThresholds::default(),
        allow_unserved: &[],
    };

    let report = assess(&input, now());
    let blocking = report.blocking();
    assert!(
        !blocking.is_empty(),
        "the 2026-09-13 state must refuse; got {:#?}",
        report.lanes
    );

    let blocked_jobs: Vec<&str> = blocking.iter().map(|lane| lane.job_id.as_str()).collect();
    assert!(
        blocked_jobs.contains(&"resolve-provider"),
        "the preamble prerequisite must be named, not just the context's own job: {blocked_jobs:?}"
    );
    assert!(
        blocked_jobs.contains(&"classify"),
        "both preamble jobs gate the context: {blocked_jobs:?}"
    );

    let rendered = report.render_refusal();
    assert!(rendered.contains("required context `macos` cannot be scheduled"));
    assert!(rendered.contains("pulp-preamble"));
    assert!(
        rendered.contains("contract [default] #4: held, not re-dispatched."),
        "the refusal must state that it did not re-dispatch: {rendered}"
    );
}

/// The control that must pass: with the variables repointed at a hosted label
/// — the live workaround in place on 2026-09-13 — nothing blocks.
///
/// Without this, a classifier that refused unconditionally would satisfy the
/// test above and be worse than useless.
#[test]
fn e1_control_hosted_routing_does_not_block() {
    let jobs = parse_workflow_jobs(BUILD_YML);
    let attestations = AttestationSet {
        hosts: vec![fresh_attestation(
            "m5",
            &[
                "self-hosted",
                "macOS",
                "ARM64",
                "pulp-build",
                "pulp-build-vm",
            ],
        )],
        unreadable: Vec::new(),
    };
    let contexts = vec!["macos".to_owned()];
    let variables = vec![
        (
            "PULP_PREAMBLE_RUNS_ON_JSON".to_owned(),
            r#"["ubuntu-latest"]"#.to_owned(),
        ),
        (
            "PULP_ALIAS_RUNS_ON_JSON".to_owned(),
            r#"["ubuntu-latest"]"#.to_owned(),
        ),
        (
            "PULP_LOCAL_MACOS_RUNS_ON_JSON".to_owned(),
            r#"["self-hosted","macOS","ARM64","pulp-build","pulp-build-vm"]"#.to_owned(),
        ),
    ];
    let census = incident_census();
    let input = AssessInput {
        contexts: &contexts,
        contexts_source: "branch_protection",
        jobs: &jobs,
        variables: &variables,
        census: &census,
        census_boundary: None,
        attestations: &attestations,
        thresholds: LaneServiceThresholds::default(),
        allow_unserved: &[],
    };
    let report = assess(&input, now());
    assert!(
        report.blocking().is_empty(),
        "healthy routing must not refuse: {:#?}",
        report.blocking()
    );
    assert!(
        !report.lanes.is_empty(),
        "a run that assessed zero lanes proves nothing; the instrument would be dead"
    );
}

// ---------------------------------------------------------------------------
// E1b — the closure is the check. A producer-only check passes the incident.
// ---------------------------------------------------------------------------

#[test]
fn e1b_context_closure_includes_the_preamble_jobs() {
    let jobs = parse_workflow_jobs(BUILD_YML);
    let producers: Vec<String> = jobs
        .iter()
        .filter(|job| job.produces("macos"))
        .map(|job| job.id.clone())
        .collect();
    assert!(
        producers.contains(&"macos".to_owned()),
        "alias job renders to `macos`: {producers:?}"
    );
    assert!(
        producers.contains(&"build".to_owned()),
        "the matrix leg also renders to `macos`: {producers:?}"
    );
    let closure = transitive_needs(&jobs, &producers);
    for want in ["resolve-provider", "classify"] {
        assert!(
            closure.contains(&want.to_owned()),
            "`{want}` gates the required context and must be in the closure: {closure:?}"
        );
    }
    assert!(
        !closure.contains(&"windows-msvc-release-gate".to_owned()),
        "the closure must not be the whole workflow: {closure:?}"
    );
}

// ---------------------------------------------------------------------------
// E2 — census scope. The repo scope alone reports an org-served lane unserved.
// ---------------------------------------------------------------------------

#[test]
fn e2_org_scope_runner_serves_a_lane_the_repo_scope_cannot_see() {
    let org_only = vec![RegisteredRunner {
        name: "pulp-intel-macmini".to_owned(),
        scope: RunnerScope::Org,
        online: true,
        busy: false,
        labels: vec![
            "self-hosted".to_owned(),
            "macOS".to_owned(),
            "X64".to_owned(),
            "pulp-intel-native".to_owned(),
        ],
    }];
    let raw = r#"["self-hosted","macOS","X64","pulp-intel-native"]"#;

    let with_org = assess_lane_service(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        raw,
        &org_only,
        None,
        &[],
        LaneServiceThresholds::default(),
        now(),
    );
    assert_eq!(with_org.verdict.as_str(), "served");
    assert!(with_org.served_only_by_org_scope());

    // The break: drop the org half. Same instrument, same target, empty answer.
    let repo_only: Vec<RegisteredRunner> = Vec::new();
    let attestations = AttestationSet {
        hosts: vec![fresh_attestation("m5", &["self-hosted", "macOS", "ARM64"])],
        unreadable: Vec::new(),
    };
    let without_org = assess_lane_service(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        raw,
        &repo_only,
        None,
        &[],
        LaneServiceThresholds::default(),
        now(),
    );
    let (verdict, _, _) = fold_attestation(&without_org, &attestations, now());
    assert_eq!(
        verdict,
        Schedulability::Unserved,
        "dropping the org census must turn a served lane into a refusal - that is why the \
         second call is not optional"
    );
}

// ---------------------------------------------------------------------------
// E2b — a half-read census is an unreadable census, not an empty one.
// ---------------------------------------------------------------------------

#[test]
fn e2b_unreadable_census_is_unknown_never_unserved() {
    let attestations = AttestationSet {
        hosts: vec![fresh_attestation("m5", &["self-hosted", "macOS", "ARM64"])],
        unreadable: Vec::new(),
    };
    let report = assess_lane_service(
        "PULP_PREAMBLE_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-preamble"]"#,
        &[],
        Some(Boundary::Scope),
        &[],
        LaneServiceThresholds::default(),
        now(),
    );
    let (verdict, _, _) = fold_attestation(&report, &attestations, now());
    assert_eq!(verdict, Schedulability::Unknown);
    assert!(
        !verdict.blocks(),
        "a blind instrument must not block a ship"
    );
}

// ---------------------------------------------------------------------------
// E7-reader — attestation coverage, staleness, and the delegation checkmark.
// ---------------------------------------------------------------------------

#[test]
fn attestation_covers_a_jit_lane_so_an_empty_census_stays_idle() {
    let attestations = AttestationSet {
        hosts: vec![fresh_attestation(
            "m5",
            &[
                "self-hosted",
                "macOS",
                "ARM64",
                "pulp-build",
                "pulp-build-vm",
            ],
        )],
        unreadable: Vec::new(),
    };
    let report = assess_lane_service(
        "PULP_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build","pulp-build-vm"]"#,
        &[],
        None,
        &[],
        LaneServiceThresholds::default(),
        now(),
    );
    assert_eq!(report.verdict.as_str(), "idle");
    let (verdict, attested_by, _) = fold_attestation(&report, &attestations, now());
    assert_eq!(verdict, Schedulability::Idle);
    assert_eq!(attested_by, vec!["m5".to_owned()]);
}

#[test]
fn a_crash_looping_persistent_runner_is_a_named_fault_not_a_checkmark() {
    // The exact record the delegation line covered for: plist present, loaded,
    // 3,684 launches, `.runner` absent, pre-org-move repo slug.
    let attestation = HostAttestation {
        schema: 1,
        host: "m5".to_owned(),
        written_at: Some(now() - chrono::Duration::seconds(10)),
        interval_secs: Some(300),
        generation: Generation::default(),
        launchd_readable: Some(true),
        profile_readable: Some(true),
        profile_detail: None,
        persistent_runners: vec![PersistentRunner {
            label: "actions.runner.danielraffel-pulp.pulp-preamble-m5".to_owned(),
            declared: true,
            installed: true,
            loaded: true,
            state: "spawn scheduled".to_owned(),
            runs: 3684,
            crash_loop: true,
            registered: false,
            registration_repo: None,
            advertises: vec![
                "self-hosted".to_owned(),
                "macOS".to_owned(),
                "ARM64".to_owned(),
                "pulp-preamble".to_owned(),
            ],
            verdict: "broken".to_owned(),
            reason: "crash loop (3684 spawns); .runner missing".to_owned(),
        }],
        jit_lanes: Vec::new(),
    };
    let set = AttestationSet {
        hosts: vec![attestation],
        unreadable: Vec::new(),
    };
    let labels = vec![
        "self-hosted".to_owned(),
        "macOS".to_owned(),
        "ARM64".to_owned(),
        "pulp-preamble".to_owned(),
    ];
    match set.coverage("m5", &labels) {
        LaneCoverage::Broken { detail } => {
            assert!(detail.contains("crash loop"), "{detail}");
            assert!(detail.contains("3684"), "{detail}");
        }
        other => panic!("a crash-looping unregistered runner must not cover a lane: {other:?}"),
    }
}

#[test]
fn a_writer_that_could_not_read_the_profile_is_not_evidence() {
    // The bug the first deployment shipped: launchd's /usr/bin/python3 is 3.9,
    // has no tomllib, and the profile parsed as empty. The record was fresh,
    // well-formed, and declared zero lanes. Counting it would let a blind
    // sensor vouch for lanes it never looked at - and, worse here, its silence
    // about a lane would read as "no host declares this" and REFUSE a ship.
    let mut attestation = fresh_attestation("m5", &["self-hosted", "macOS", "ARM64"]);
    attestation.profile_readable = Some(false);
    attestation.profile_detail = Some("python 3.9.6 has no tomllib".to_owned());
    attestation.jit_lanes.clear();
    let set = AttestationSet {
        hosts: vec![attestation],
        unreadable: Vec::new(),
    };
    assert!(
        set.fresh_hosts(now()).is_empty(),
        "a record written by a writer that could not read the profile is not evidence"
    );
    assert!(set.describe_staleness(now()).contains("BLIND"));
}

#[test]
fn a_stale_attestation_is_not_evidence() {
    let mut attestation = fresh_attestation("m5", &["self-hosted", "macOS", "ARM64"]);
    attestation.written_at = Some(now() - chrono::Duration::seconds(3601));
    let set = AttestationSet {
        hosts: vec![attestation],
        unreadable: Vec::new(),
    };
    assert!(set.fresh_hosts(now()).is_empty());

    let report = assess_lane_service(
        "PULP_PREAMBLE_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-preamble"]"#,
        &[],
        None,
        &[],
        LaneServiceThresholds::default(),
        now(),
    );
    let (verdict, _, faults) = fold_attestation(&report, &set, now());
    assert_eq!(
        verdict,
        Schedulability::Unknown,
        "with no fresh attestation the answer is Unknown, never Unserved and never Served"
    );
    assert!(
        faults
            .iter()
            .any(|fault| fault.contains("written 3601s ago")),
        "the fault must name the age that disqualified it: {faults:?}"
    );
}

// ---------------------------------------------------------------------------
// Expression resolver.
// ---------------------------------------------------------------------------

#[test]
fn runs_on_expression_forms() {
    assert_eq!(
        resolve_runs_on_expr("windows-latest"),
        RunsOnResolution::Literal {
            value: "windows-latest".to_owned()
        }
    );
    assert_eq!(
        resolve_runs_on_expr(
            r#"${{ fromJSON(vars.PULP_PREAMBLE_RUNS_ON_JSON || '"ubuntu-latest"') }}"#
        ),
        RunsOnResolution::Variable {
            name: "PULP_PREAMBLE_RUNS_ON_JSON".to_owned(),
            fallback: Some("\"ubuntu-latest\"".to_owned()),
        }
    );
    assert_eq!(
        resolve_runs_on_expr("${{ fromJSON(vars.PULP_ALIAS_RUNS_ON_JSON) }}"),
        RunsOnResolution::Variable {
            name: "PULP_ALIAS_RUNS_ON_JSON".to_owned(),
            fallback: None,
        }
    );
    match resolve_runs_on_expr("${{ fromJSON(needs.resolve-provider.outputs.macos_runs_on_json) }}")
    {
        RunsOnResolution::Dynamic { from_jobs, .. } => {
            assert_eq!(from_jobs, vec!["resolve-provider".to_owned()]);
        }
        other => panic!("job-output form must be Dynamic: {other:?}"),
    }
    // Anything else must be loud, not guessed at.
    match resolve_runs_on_expr("${{ steps.pick.outputs.runner }}") {
        RunsOnResolution::Unparsable { .. } => {}
        other => panic!("an unrecognized expression must be Unparsable: {other:?}"),
    }
}

#[test]
fn workflow_parser_reads_the_routing_facts() {
    let jobs = parse_workflow_jobs(BUILD_YML);
    let ids: Vec<&str> = jobs.iter().map(|job| job.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "resolve-provider",
            "classify",
            "build",
            "macos",
            "windows-msvc-release-gate"
        ]
    );
    let build = jobs.iter().find(|job| job.id == "build").unwrap();
    assert_eq!(
        build.needs,
        vec!["resolve-provider".to_owned(), "classify".to_owned()],
        "hyphenated job ids must survive `needs` parsing"
    );
    let resolve = jobs
        .iter()
        .find(|job| job.id == "resolve-provider")
        .unwrap();
    assert!(
        resolve
            .var_refs
            .contains(&"PULP_LOCAL_MACOS_RUNS_ON_JSON".to_owned())
    );
    assert!(
        resolve
            .var_refs
            .contains(&"PULP_OVERFLOW_BUILD_MACOS_RUNS_ON_JSON".to_owned())
    );
}

// ---------------------------------------------------------------------------
// API budget. The number is measured, not estimated: a budget nobody measures
// is a wish, and GitHub's secondary limit trips on burst shape rather than on
// quota.
// ---------------------------------------------------------------------------

#[test]
fn a_warm_cache_costs_zero_api_calls() {
    use super::gather::{CACHE_TTL_SECS, cache_path, gather};
    use crate::cloud::GitHubActions;

    let dir = tempfile::tempdir().expect("tempdir");
    let state = dir.path();
    // Seed the cache the way a prior cold gather would have.
    let seeded = serde_json::json!({
        "fetched_at": now(),
        "repo": "Generous-Corp/pulp",
        "base": "main",
        "variables": [["PULP_PREAMBLE_RUNS_ON_JSON", "[\"ubuntu-latest\"]"]],
        "census": [],
        "census_boundary": serde_json::Value::Null,
        "required_contexts": ["macos"],
        "contexts_source": "branch_protection",
    });
    std::fs::write(
        cache_path(state, "Generous-Corp/pulp"),
        serde_json::to_string(&seeded).expect("serialize"),
    )
    .expect("write cache");

    // A client pointed at a directory with no `gh` configured: if the cache
    // were missed, the calls would be attempted and counted. They are not.
    let actions = GitHubActions::new(dir.path());
    let facts = gather(
        &actions,
        state,
        "Generous-Corp/pulp",
        "main",
        now() + chrono::Duration::seconds(CACHE_TTL_SECS - 1),
        false,
    );
    assert_eq!(
        facts.api_calls, 0,
        "a warm cache must cost nothing; this is what makes the gate cheap enough to run on \
         every ship"
    );
    assert_eq!(facts.required_contexts, Some(vec!["macos".to_owned()]));

    // The control: one second past the TTL the cache must be ignored, or the
    // assertion above is satisfied by a cache that never expires — which would
    // make the gate report a runner census from an arbitrarily distant past.
    let cold = gather(
        &actions,
        state,
        "Generous-Corp/pulp",
        "main",
        now() + chrono::Duration::seconds(CACHE_TTL_SECS + 1),
        false,
    );
    assert!(
        cold.api_calls > 0,
        "past the TTL the facts must be re-fetched, not served stale"
    );
}
