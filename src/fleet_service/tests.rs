//! Tests for the fleet service assertions.
//!
//! The label sets, runner names and variable names below are the real ones
//! observed on the fleet these assertions were written against, so each
//! incident fixture reproduces the shape of the outage rather than an
//! idealisation of it.
//!
//! Every assertion carries a **planted negative control that must go red**: for
//! each incident, the healthy census is asserted to produce a passing verdict
//! and the incident census is asserted to produce a raising one. A detector
//! that cannot fail its own test is precisely the failure mode this module
//! exists to end, so the pairs are kept adjacent and named for it.

use chrono::{Duration, TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 23, 44, 0).unwrap()
}

fn labels(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn runner(name: &str, scope: RunnerScope, online: bool, advertised: &[&str]) -> RegisteredRunner {
    RegisteredRunner {
        name: name.to_owned(),
        scope,
        online,
        busy: false,
        labels: labels(advertised),
    }
}

fn demand_aged(advertised: &[&str], minutes: i64) -> QueuedDemand {
    QueuedDemand {
        labels: labels(advertised),
        queued_since: now() - Duration::minutes(minutes),
    }
}

fn assess(
    variable: &str,
    raw: &str,
    census: &[RegisteredRunner],
    demand: &[QueuedDemand],
) -> LaneReport {
    assess_lane_service(
        variable,
        raw,
        census,
        None,
        demand,
        LaneServiceThresholds::default(),
        now(),
    )
}

// ---------------------------------------------------------------------------
// Routing-value parsing — three live encodings share one namespace
// ---------------------------------------------------------------------------

#[test]
fn parses_a_json_array_of_labels_as_a_self_hosted_lane() {
    let parsed = parse_runs_on(r#"["self-hosted","macOS","ARM64","pulp-build-vm-release"]"#);
    assert_eq!(
        parsed,
        LaneDeclaration::SelfHosted {
            labels: labels(&["self-hosted", "macOS", "ARM64", "pulp-build-vm-release"]),
        }
    );
}

#[test]
fn parses_a_json_string_as_a_hosted_lane() {
    assert_eq!(
        parse_runs_on(r#""macos-15""#),
        LaneDeclaration::Hosted {
            labels: labels(&["macos-15"]),
        }
    );
}

#[test]
fn parses_a_bare_unquoted_string_as_a_hosted_lane() {
    // `PULP_COVERAGE_MACOS_RUNS_ON_JSON` is stored without JSON quoting on the
    // live fleet. A parser that assumes valid JSON drops this lane silently.
    assert_eq!(
        parse_runs_on("macos-15"),
        LaneDeclaration::Hosted {
            labels: labels(&["macos-15"]),
        }
    );
}

#[test]
fn parses_a_routing_sentinel_as_naming_no_runner() {
    assert_eq!(
        parse_runs_on("local-only"),
        LaneDeclaration::Sentinel {
            value: "local-only".to_owned(),
        }
    );
}

#[test]
fn parses_a_single_element_hosted_array() {
    assert_eq!(
        parse_runs_on(r#"["ubuntu-latest"]"#),
        LaneDeclaration::Hosted {
            labels: labels(&["ubuntu-latest"]),
        }
    );
}

#[test]
fn refuses_to_guess_at_a_non_string_array_member() {
    assert_eq!(
        parse_runs_on(r#"["self-hosted",42]"#),
        LaneDeclaration::Unparsable {
            raw: r#"["self-hosted",42]"#.to_owned(),
        }
    );
}

#[test]
fn an_empty_value_is_unparsable_not_an_empty_lane() {
    assert_eq!(
        parse_runs_on("   "),
        LaneDeclaration::Unparsable {
            raw: "   ".to_owned(),
        }
    );
}

// ---------------------------------------------------------------------------
// Incident 1 — the Intel lane: scope blindness, same data, opposite verdicts
// ---------------------------------------------------------------------------

const INTEL_LANE: &str = r#"["self-hosted","macOS","X64","pulp-intel-native","pulp-host-macmini"]"#;

fn intel_runner() -> RegisteredRunner {
    runner(
        "pulp-intel-macmini",
        RunnerScope::Org,
        true,
        &[
            "self-hosted",
            "X64",
            "macOS",
            "pulp-intel-native",
            "pulp-host-macmini",
            "native-intel-advisory",
        ],
    )
}

fn intel_demand(minutes: i64) -> QueuedDemand {
    demand_aged(
        &[
            "self-hosted",
            "macOS",
            "X64",
            "pulp-intel-native",
            "pulp-host-macmini",
        ],
        minutes,
    )
}

/// `repos/{owner}/{repo}/actions/runners` omits org-registered runners
/// entirely. A lane served only at the org scope must not read as unserved.
#[test]
fn a_lane_served_only_at_the_org_scope_reads_served() {
    let report = assess(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        INTEL_LANE,
        &[intel_runner()],
        &[],
    );

    assert_eq!(report.verdict, ServiceVerdict::Served, "{}", report.detail);
    assert!(!report.verdict.is_raise());
    assert!(report.served_only_by_org_scope());
    assert!(
        report.detail.contains("org scope only"),
        "the output must say the repo-scope census would have missed this: {}",
        report.detail
    );
}

/// The planted control for scope blindness, run as a matched pair so that the
/// **only** difference between the two assessments is which scopes the census
/// spans. Identical lane, identical aged demand.
///
/// This is the shape of the real harm: the repo-scope census does not merely
/// miss the runner, it manufactures a confident `Unserved` verdict that would
/// send an operator to repair routing which was correct all along — the exact
/// same empty reading it gives when a host is genuinely dead.
#[test]
fn negative_control_a_repo_only_census_manufactures_a_false_unserved_verdict() {
    let demand = [intel_demand(120)];

    let both_scopes = assess(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        INTEL_LANE,
        &[intel_runner()],
        &demand,
    );
    let repo_only = assess("PULP_NATIVE_INTEL_RUNS_ON_JSON", INTEL_LANE, &[], &demand);

    assert_ne!(
        both_scopes.verdict,
        ServiceVerdict::Unserved,
        "an online org-scope runner advertises this lane: {}",
        both_scopes.detail
    );
    assert_eq!(both_scopes.matches.len(), 1);
    assert!(both_scopes.served_only_by_org_scope());

    assert_eq!(
        repo_only.verdict,
        ServiceVerdict::Unserved,
        "{}",
        repo_only.detail
    );
    assert!(repo_only.verdict.is_raise());
    assert!(repo_only.matches.is_empty());
}

/// With an online runner present, aged demand is `Starved`, never `Unserved` —
/// the routing is right and the operator should be sent to capacity instead.
#[test]
fn an_org_served_lane_with_aged_demand_is_starved_not_unserved() {
    let report = assess(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        INTEL_LANE,
        &[intel_runner()],
        &[intel_demand(120)],
    );

    assert_eq!(report.verdict, ServiceVerdict::Starved, "{}", report.detail);
}

// ---------------------------------------------------------------------------
// Incident 2 — macpro: a Linux lane unserved for nineteen days
// ---------------------------------------------------------------------------

#[test]
fn the_linux_lane_reads_served_when_its_ephemeral_clones_are_registered() {
    let census = [
        runner(
            "pulp-auto-ephemeral-200",
            RunnerScope::Org,
            true,
            &[
                "self-hosted",
                "X64",
                "Linux",
                "pulp-build-linux-x64",
                "pulp-host-macpro",
                "pulp-auto-linux-x64",
            ],
        ),
        runner(
            "pulp-auto-ephemeral-201",
            RunnerScope::Org,
            true,
            &[
                "self-hosted",
                "X64",
                "Linux",
                "pulp-build-linux-x64",
                "pulp-host-macpro",
                "pulp-auto-linux-x64",
            ],
        ),
    ];
    let report = assess(
        "PULP_AUTO_LINUX_RUNS_ON_JSON",
        r#"["self-hosted","Linux","X64","pulp-build-linux-x64","pulp-host-macpro","pulp-auto-linux-x64"]"#,
        &census,
        &[demand_aged(
            &["self-hosted", "Linux", "X64", "pulp-build-linux-x64"],
            3,
        )],
    );

    assert_eq!(report.verdict, ServiceVerdict::Served, "{}", report.detail);
    assert_eq!(report.matches.len(), 2);
}

/// Planted negative control: the pool's clones are orphaned, so nothing
/// registers, while Linux jobs keep queueing. This is the state that persisted
/// for nineteen days behind an `active (running)` unit and a green hosted
/// fallback.
#[test]
fn negative_control_an_orphaned_pool_leaves_the_linux_lane_unserved() {
    let report = assess(
        "PULP_AUTO_LINUX_RUNS_ON_JSON",
        r#"["self-hosted","Linux","X64","pulp-build-linux-x64","pulp-host-macpro","pulp-auto-linux-x64"]"#,
        &[],
        &[demand_aged(
            &[
                "self-hosted",
                "Linux",
                "X64",
                "pulp-build-linux-x64",
                "pulp-host-macpro",
                "pulp-auto-linux-x64",
            ],
            19 * 24 * 60,
        )],
    );

    assert_eq!(
        report.verdict,
        ServiceVerdict::Unserved,
        "{}",
        report.detail
    );
    assert!(report.verdict.is_raise());
    assert!(
        report.detail.contains("served by nobody"),
        "{}",
        report.detail
    );
}

// ---------------------------------------------------------------------------
// Incident 3 — a variable asking for a label the fleet stopped minting
// ---------------------------------------------------------------------------

/// The fleet migrated off `pulp-gate-fast` deliberately; the repo half of that
/// migration never shipped. Nothing advertises the label, so the intersection
/// is empty and the job waits forever. Unschedulable is not "slow".
#[test]
fn negative_control_a_label_nothing_mints_reads_unserved_not_degraded() {
    let census = [
        runner(
            "studio-pulp-gate-01-65248-25",
            RunnerScope::Repo,
            true,
            &[
                "self-hosted",
                "macOS",
                "ARM64",
                "pulp-build",
                "pulp-build-vm",
                "pulp-build-pr-head",
            ],
        ),
        runner(
            "m5-pulp-gate-slot2-02-51603-19",
            RunnerScope::Repo,
            true,
            &[
                "self-hosted",
                "macOS",
                "ARM64",
                "pulp-build",
                "pulp-build-vm",
                "pulp-build-pr-head",
            ],
        ),
    ];
    let report = assess(
        "PULP_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build","pulp-build-vm","pulp-gate-fast"]"#,
        &census,
        &[demand_aged(
            &[
                "self-hosted",
                "macOS",
                "ARM64",
                "pulp-build",
                "pulp-build-vm",
                "pulp-gate-fast",
            ],
            7 * 60,
        )],
    );

    assert_eq!(
        report.verdict,
        ServiceVerdict::Unserved,
        "{}",
        report.detail
    );
    assert!(
        report.matches.is_empty(),
        "two online gate runners exist but neither advertises pulp-gate-fast"
    );
}

/// Control for the test above: the *same* census serves the gate lane that asks
/// only for labels the fleet still mints. If this went red too, the fixture
/// would be proving nothing about the missing label.
#[test]
fn the_same_census_serves_the_lane_that_asks_only_for_minted_labels() {
    let census = [runner(
        "studio-pulp-gate-01-65248-25",
        RunnerScope::Repo,
        true,
        &[
            "self-hosted",
            "macOS",
            "ARM64",
            "pulp-build",
            "pulp-build-vm",
            "pulp-build-pr-head",
        ],
    )];
    let report = assess(
        "PULP_GATE_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build","pulp-build-vm","pulp-build-pr-head"]"#,
        &census,
        &[],
    );

    assert_eq!(report.verdict, ServiceVerdict::Served, "{}", report.detail);
}

// ---------------------------------------------------------------------------
// The distinctions the taxonomy exists for
// ---------------------------------------------------------------------------

/// An idle just-in-time pool registers nothing. Reporting that as a fault is
/// the reading that was filed as an issue and closed as wrong, so an empty
/// census with no demand must not raise.
#[test]
fn an_empty_census_with_no_demand_is_idle_not_unserved() {
    let report = assess(
        "PULP_RELEASE_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm-release"]"#,
        &[],
        &[],
    );

    assert_eq!(report.verdict, ServiceVerdict::Idle, "{}", report.detail);
    assert!(!report.verdict.is_raise());
}

#[test]
fn demand_younger_than_the_threshold_stays_idle() {
    let report = assess(
        "PULP_RELEASE_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm-release"]"#,
        &[],
        &[demand_aged(
            &["self-hosted", "macOS", "ARM64", "pulp-build-vm-release"],
            2,
        )],
    );

    assert_eq!(report.verdict, ServiceVerdict::Idle, "{}", report.detail);
    assert_eq!(report.demand_count, 1);
}

/// Planted negative control for the pair above: the same empty census, the same
/// pool, but the demand has aged past its threshold. A pool at rest and a pool
/// that stopped booting look identical until demand is weighed.
#[test]
fn negative_control_aged_demand_turns_the_same_idle_pool_unserved() {
    let report = assess(
        "PULP_RELEASE_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm-release"]"#,
        &[],
        &[demand_aged(
            &["self-hosted", "macOS", "ARM64", "pulp-build-vm-release"],
            48,
        )],
    );

    assert_eq!(
        report.verdict,
        ServiceVerdict::Unserved,
        "{}",
        report.detail
    );
    assert_eq!(report.oldest_demand_secs, Some(48 * 60));
}

/// A lane whose labels *are* advertised by an online runner, yet whose jobs sit
/// queued, is a different fault with a different remedy. Merging it into
/// `Unserved` would send an operator to fix routing that is already correct.
#[test]
fn aged_demand_against_an_online_runner_is_starved_not_unserved() {
    let census = [runner(
        "studio-pulp-gate-01-65248-25",
        RunnerScope::Repo,
        true,
        &["self-hosted", "macOS", "ARM64", "pulp-build-vm"],
    )];
    let report = assess(
        "PULP_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm"]"#,
        &census,
        &[demand_aged(
            &["self-hosted", "macOS", "ARM64", "pulp-build-vm"],
            90,
        )],
    );

    assert_eq!(report.verdict, ServiceVerdict::Starved, "{}", report.detail);
    assert!(report.verdict.is_raise());
    assert!(
        report.detail.contains("scheduling or capacity"),
        "{}",
        report.detail
    );
}

/// A registered-but-offline runner cannot serve. It must still be distinguished
/// in the detail from "no runner exists", because the remedies differ: bring a
/// host up versus fix a label.
#[test]
fn matches_that_are_all_offline_read_unserved_and_say_so() {
    let census = [runner(
        "studio-forge-gate-01-94107-1",
        RunnerScope::Org,
        false,
        &["self-hosted", "macOS", "ARM64", "forge-build-vm"],
    )];
    let report = assess(
        "FORGE_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","forge-build-vm"]"#,
        &census,
        &[demand_aged(
            &["self-hosted", "macOS", "ARM64", "forge-build-vm"],
            60,
        )],
    );

    assert_eq!(
        report.verdict,
        ServiceVerdict::Unserved,
        "{}",
        report.detail
    );
    assert_eq!(report.matches.len(), 1);
    assert!(
        report.detail.contains("every one is offline"),
        "{}",
        report.detail
    );
}

/// Demand is matched in the job's direction: a queued job counts as demand for
/// a lane only when it requests **every** label that lane declares. A job
/// asking for a subset is routed by a different variable and may be perfectly
/// well served elsewhere, so counting it here would attribute one lane's
/// backlog to another and raise on the wrong host.
#[test]
fn a_queued_job_requesting_only_a_subset_is_not_demand_for_this_lane() {
    let report = assess(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        r#"["self-hosted","macOS","X64","pulp-intel-native","pulp-host-macmini"]"#,
        &[],
        &[demand_aged(&["self-hosted", "macOS", "X64"], 120)],
    );

    assert_eq!(report.demand_count, 0);
    assert_eq!(report.verdict, ServiceVerdict::Idle, "{}", report.detail);
}

/// The converse: a job requesting a superset does belong to this lane, since
/// every label the lane declares must still be satisfied for it to schedule.
#[test]
fn a_queued_job_requesting_a_superset_is_demand_for_this_lane() {
    let report = assess(
        "PULP_NATIVE_INTEL_RUNS_ON_JSON",
        r#"["self-hosted","macOS","X64","pulp-intel-native","pulp-host-macmini"]"#,
        &[],
        &[demand_aged(
            &[
                "self-hosted",
                "macOS",
                "X64",
                "pulp-intel-native",
                "pulp-host-macmini",
                "native-intel-advisory",
            ],
            120,
        )],
    );

    assert_eq!(report.demand_count, 1);
    assert_eq!(
        report.verdict,
        ServiceVerdict::Unserved,
        "{}",
        report.detail
    );
}

#[test]
fn label_matching_ignores_case_the_way_github_does() {
    let census = [runner(
        "case-drift",
        RunnerScope::Repo,
        true,
        &["Self-Hosted", "MACOS", "arm64", "pulp-build-vm"],
    )];
    let report = assess(
        "PULP_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm"]"#,
        &census,
        &[],
    );

    assert_eq!(report.verdict, ServiceVerdict::Served, "{}", report.detail);
}

#[test]
fn a_runner_missing_one_required_label_does_not_match() {
    let census = [runner(
        "partial",
        RunnerScope::Repo,
        true,
        &["self-hosted", "macOS", "ARM64"],
    )];
    let report = assess(
        "PULP_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm"]"#,
        &census,
        &[],
    );

    assert!(report.matches.is_empty());
    assert_eq!(report.verdict, ServiceVerdict::Idle, "{}", report.detail);
}

// ---------------------------------------------------------------------------
// The instrument's own health is never folded into a pass
// ---------------------------------------------------------------------------

/// An unreadable census produces the same empty vector as a genuinely empty
/// one. Only the caller knows which it had, so it must say, and the verdict
/// must not be a pass.
#[test]
fn negative_control_an_unreadable_census_is_unknown_never_served() {
    let report = assess_lane_service(
        "PULP_AUTO_LINUX_RUNS_ON_JSON",
        r#"["self-hosted","Linux","X64","pulp-build-linux-x64"]"#,
        &[],
        Some(Boundary::Transport),
        &[],
        LaneServiceThresholds::default(),
        now(),
    );

    assert_eq!(report.verdict, ServiceVerdict::Unknown, "{}", report.detail);
    assert!(report.verdict.is_raise());
    assert_eq!(report.boundary, Some(Boundary::Transport));
}

#[test]
fn negative_control_an_unparsable_declaration_is_unknown_never_served() {
    let report = assess("PULP_BROKEN_RUNS_ON_JSON", "", &[], &[]);
    assert_eq!(report.verdict, ServiceVerdict::Unknown, "{}", report.detail);
    assert!(report.verdict.is_raise());
    assert_eq!(report.boundary, Some(Boundary::Parse));
}

/// Every unknown must name its boundary, and no measured verdict may claim one.
/// An unknown that cannot say why is the defect this field exists to prevent.
#[test]
fn a_boundary_is_present_exactly_when_the_verdict_is_unknown() {
    for boundary in [
        Boundary::Grammar,
        Boundary::Scope,
        Boundary::Identity,
        Boundary::Permission,
        Boundary::Parse,
        Boundary::Transport,
    ] {
        let report = assess_lane_service(
            "PULP_AUTO_LINUX_RUNS_ON_JSON",
            r#"["self-hosted","Linux","X64","pulp-build-linux-x64"]"#,
            &[],
            Some(boundary),
            &[],
            LaneServiceThresholds::default(),
            now(),
        );
        assert_eq!(report.verdict, ServiceVerdict::Unknown);
        assert_eq!(report.boundary, Some(boundary));
        assert!(
            report.detail.contains(boundary.as_str()),
            "the message must name the boundary it hit: {}",
            report.detail
        );
    }

    let measured = assess(
        "PULP_LOCAL_MACOS_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm"]"#,
        &[runner(
            "ok",
            RunnerScope::Repo,
            true,
            &["self-hosted", "macOS", "ARM64", "pulp-build-vm"],
        )],
        &[],
    );
    assert_eq!(measured.verdict, ServiceVerdict::Served);
    assert_eq!(measured.boundary, None);
}

/// The distinction the second remit turns on: only a genuine permission fault
/// means "you cannot". A grammar refusal, a wrong scope, a wrong identity and a
/// failed call all have doors in them, and reporting them as "cannot" is what
/// makes a reader stop at a wall it could have walked through.
#[test]
fn only_a_permission_boundary_denies_that_an_equivalent_path_exists() {
    assert!(!Boundary::Permission.equivalent_path_may_exist());
    for boundary in [
        Boundary::Grammar,
        Boundary::Scope,
        Boundary::Identity,
        Boundary::Parse,
        Boundary::Transport,
    ] {
        assert!(
            boundary.equivalent_path_may_exist(),
            "{} must not read as a permission denial",
            boundary.as_str()
        );
    }
}

/// A timeout is not an authentication fault. The supervisor that logged
/// `self-restarting for fresh gh auth` for a scan timeout took a corrective
/// action that could not help, and the message is what sent it there.
#[test]
fn a_transport_boundary_tells_the_reader_not_to_re_authenticate() {
    assert!(
        Boundary::Transport
            .next_action()
            .contains("not an authentication"),
        "{}",
        Boundary::Transport.next_action()
    );
}

#[test]
fn boundary_strings_are_stable() {
    assert_eq!(Boundary::Grammar.as_str(), "grammar");
    assert_eq!(Boundary::Scope.as_str(), "scope");
    assert_eq!(Boundary::Identity.as_str(), "identity");
    assert_eq!(Boundary::Permission.as_str(), "permission");
    assert_eq!(Boundary::Parse.as_str(), "parse");
    assert_eq!(Boundary::Transport.as_str(), "transport");
}

// ---------------------------------------------------------------------------
// Lanes this fleet does not serve
// ---------------------------------------------------------------------------

#[test]
fn a_hosted_lane_is_not_this_fleets_problem() {
    let report = assess(
        "PULP_SANITIZER_UBSAN_RUNS_ON_JSON",
        r#""macos-26""#,
        &[],
        &[],
    );
    assert_eq!(report.verdict, ServiceVerdict::Served, "{}", report.detail);
    assert!(report.detail.contains("served by GitHub"));
}

#[test]
fn a_routing_sentinel_is_not_a_lane() {
    let report = assess(
        "PULP_OVERFLOW_BUILD_MACOS_RUNS_ON_JSON",
        "local-only",
        &[],
        &[],
    );
    assert_eq!(report.verdict, ServiceVerdict::Served, "{}", report.detail);
    assert!(report.detail.contains("names no runner label"));
}

// ---------------------------------------------------------------------------
// Roll-up
// ---------------------------------------------------------------------------

#[test]
fn severity_ordering_puts_unknown_above_every_measured_fault() {
    assert!(ServiceVerdict::Unknown > ServiceVerdict::Unserved);
    assert!(ServiceVerdict::Unserved > ServiceVerdict::Starved);
    assert!(ServiceVerdict::Starved > ServiceVerdict::Degraded);
    assert!(ServiceVerdict::Degraded > ServiceVerdict::Idle);
    assert!(ServiceVerdict::Idle > ServiceVerdict::Served);
}

#[test]
fn the_roll_up_takes_the_worst_verdict() {
    let healthy = assess(
        "A_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-build-vm"]"#,
        &[runner(
            "ok",
            RunnerScope::Repo,
            true,
            &["self-hosted", "macOS", "ARM64", "pulp-build-vm"],
        )],
        &[],
    );
    let broken = assess(
        "B_RUNS_ON_JSON",
        r#"["self-hosted","macOS","ARM64","pulp-gate-fast"]"#,
        &[],
        &[demand_aged(
            &["self-hosted", "macOS", "ARM64", "pulp-gate-fast"],
            120,
        )],
    );

    assert_eq!(healthy.verdict, ServiceVerdict::Served);
    assert_eq!(
        roll_up(&[healthy, broken]),
        ServiceVerdict::Unserved,
        "one unserved lane must not be averaged away by healthy siblings"
    );
}

/// Asserting nothing is not the same as asserting everything passed.
#[test]
fn negative_control_an_empty_roll_up_is_unknown_not_served() {
    assert_eq!(roll_up(&[]), ServiceVerdict::Unknown);
}

#[test]
fn verdict_strings_are_stable() {
    assert_eq!(ServiceVerdict::Served.as_str(), "served");
    assert_eq!(ServiceVerdict::Idle.as_str(), "idle");
    assert_eq!(ServiceVerdict::Degraded.as_str(), "degraded");
    assert_eq!(ServiceVerdict::Starved.as_str(), "starved");
    assert_eq!(ServiceVerdict::Unserved.as_str(), "unserved");
    assert_eq!(ServiceVerdict::Unknown.as_str(), "unknown");
    assert_eq!(RunnerScope::Repo.as_str(), "repo");
    assert_eq!(RunnerScope::Org.as_str(), "org");
}

#[test]
fn declaration_kind_strings_are_stable() {
    assert_eq!(
        parse_runs_on(r#"["self-hosted","x"]"#).kind(),
        "self_hosted"
    );
    assert_eq!(parse_runs_on(r#""macos-15""#).kind(), "hosted");
    assert_eq!(parse_runs_on("local-only").kind(), "sentinel");
    assert_eq!(parse_runs_on("").kind(), "unparsable");
}

#[test]
fn declaration_labels_are_exposed_for_reporting() {
    assert_eq!(
        parse_runs_on(r#"["self-hosted","pulp-build-vm"]"#).labels(),
        labels(&["self-hosted", "pulp-build-vm"]).as_slice()
    );
    assert!(parse_runs_on("local-only").labels().is_empty());
    assert!(parse_runs_on("").labels().is_empty());
}
