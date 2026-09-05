//! Tests for the fleet supervisor scan assertions.
//!
//! The log lines below are the literal formats the tartci macOS supervisor
//! emits, so each fixture reproduces the shape of the outage rather than an
//! idealisation of it.
//!
//! Every check ships with a **planted negative control that must go red**. A
//! detector that cannot fail its own test is exactly the failure mode this
//! module encodes, so each control is a real test with a name that states what
//! it proves:
//!
//! * the tail of the incident window is asserted to read healthy, which is what
//!   makes the full-window `Degraded` meaningful rather than a coincidence;
//! * a clean window is asserted not to raise, so `Degraded` is not the only
//!   value the assertion can produce;
//! * a timeout is asserted *not* to read as an auth fault, because reading it
//!   as one is the original bug;
//! * the same latencies are asserted to flip verdict on the budget alone.

use chrono::{TimeZone, Utc};

use super::*;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 4, 23, 44, 0).unwrap()
}

/// The blind line the supervisor prints when a queue scan does not return.
///
/// Carries two fractions: `6/9` is the consecutive-blind counter, and
/// `running_macos_vms=1/2` is the VM census. Only the first is the counter.
fn blind_line(consecutive: usize) -> String {
    format!(
        "SCAN BLIND (gh queue scan failed) {consecutive}/9 — NOT idling as empty \
         (running_macos_vms=1/2)"
    )
}

/// The blind line that precedes the supervisor's own (useless) auth restart.
const BLIND_RESTART_LINE: &str =
    "SCAN BLIND ~180s — self-restarting the supervisor for fresh gh auth";

/// A healthy cycle: the supervisor obtained a queue depth and chose to yield.
const SIGHTED_LINE: &str = "• yielding 20s (queued=2 priority_demand=2 running_macos_vms=1/2) \
                            — priority lane 'Build and Test' has the slot";

/// A line that is neither cycle shape.
const CHATTER_LINE: &str = "[tartci] supervisor tick";

fn assess(window: &SupervisorScanWindow) -> SupervisorReport {
    assess_supervisor_scan(window, ScanThresholds::default(), now())
}

fn borrowed(lines: &[String]) -> Vec<&str> {
    lines.iter().map(String::as_str).collect()
}

// ---------------------------------------------------------------------------
// The measured incident window: 2000 lines, 1598 blind, 80 sighted, healthy
// tail. Sampled at the tail it is green; measured as a ratio it is not.
// ---------------------------------------------------------------------------

/// Reproduce the shape measured on the live release supervisor: of the last
/// 2000 log lines, 1598 `SCAN BLIND`, 80 carrying `queued=`, the last blind
/// line roughly seventy lines from the end, and the final seventy healthy.
fn release_supervisor_window() -> Vec<String> {
    const TOTAL: usize = 2000;
    const BLIND: usize = 1598;
    const HEALTHY_TAIL: usize = 70;
    const EARLY_SIGHTED: usize = 10;

    let mut lines = Vec::with_capacity(TOTAL);
    let mut consecutive = 0usize;
    let mut early_sighted = 0usize;

    for index in 0..BLIND {
        consecutive = consecutive % 9 + 1;
        lines.push(blind_line(consecutive));
        // A handful of successful scans early in the window, so the fixture is
        // a degrading supervisor rather than a uniformly dead one.
        if index > 0 && index % 150 == 0 && early_sighted < EARLY_SIGHTED {
            lines.push(SIGHTED_LINE.to_owned());
            early_sighted += 1;
            consecutive = 0;
        }
    }
    assert_eq!(early_sighted, EARLY_SIGHTED);

    while lines.len() < TOTAL - HEALTHY_TAIL {
        lines.push(CHATTER_LINE.to_owned());
    }
    for _ in 0..HEALTHY_TAIL {
        lines.push(SIGHTED_LINE.to_owned());
    }

    assert_eq!(lines.len(), TOTAL, "fixture must be exactly 2000 lines");
    lines
}

#[test]
fn the_incident_window_parses_to_the_measured_counts() {
    let lines = release_supervisor_window();
    let window = parse_scan_window(&borrowed(&lines));

    assert_eq!(window.lines_scanned, 2000);
    assert_eq!(window.blind_cycles, 1598);
    assert_eq!(window.sighted_cycles, 80);
    // The window ends healthy — this zero is the reading that fooled everyone.
    assert_eq!(window.trailing_consecutive_blind, 0);
}

/// **The single most important test in this module.**
///
/// The same supervisor, the same log, two ways of looking at it. The whole
/// window is three-quarters blind and raises; its final seventy lines are
/// entirely healthy and do not. A boolean "is it blind right now?" check reads
/// the second one, which is why the outage survived a day of being looked at.
#[test]
fn a_ratio_catches_the_incident_that_a_tail_sample_calls_healthy() {
    let lines = release_supervisor_window();
    let all = borrowed(&lines);

    let whole = assess(&parse_scan_window(&all));
    assert_eq!(whole.verdict, ServiceVerdict::Degraded);
    assert!(whole.verdict.is_raise());
    assert!(
        whole.blind_ratio.expect("ratio over 1678 cycles") > 0.9,
        "1598 of 1678 cycles blind is over 95%, got {:?}",
        whole.blind_ratio
    );
    assert!(
        whole.detail.contains("ENDS healthy"),
        "the message must say the tail reads green: {}",
        whole.detail
    );
    // The raise must be ATTRIBUTED to the ratio. Without this the test passes
    // on the run-length signal alone, which would let a broken ratio ship — the
    // mutation that proved it is the reason this assertion exists.
    assert!(
        whole.detail.contains("scan cycles blind") && whole.detail.contains("budget 5.0%"),
        "the ratio must be the named cause, not merely a coincident one: {}",
        whole.detail
    );
}

/// The planted negative control for the test above: prove the tail really does
/// read healthy, so the `Degraded` verdict is attributable to the ratio and
/// not to something the fixture happens to contain everywhere.
#[test]
fn control_the_tail_of_the_incident_window_reads_healthy_on_its_own() {
    let lines = release_supervisor_window();
    let all = borrowed(&lines);
    let tail = &all[all.len() - 70..];

    let sampled = assess(&parse_scan_window(tail));
    assert_eq!(
        sampled.verdict,
        ServiceVerdict::Served,
        "a 70-line tail sample of a supervisor that is 95% blind must read Served, \
         or this test proves nothing: {}",
        sampled.detail
    );
    assert!(!sampled.verdict.is_raise());
    assert_eq!(sampled.blind_cycles, 0);
}

/// Isolate the ratio from the run-length signal. With the consecutive-blind
/// budget raised out of reach, the ratio is the *only* thing left that can
/// raise — and it still does.
#[test]
fn control_the_ratio_alone_still_catches_the_incident() {
    let lines = release_supervisor_window();
    let window = parse_scan_window(&borrowed(&lines));

    let ratio_only = ScanThresholds {
        max_consecutive_blind: usize::MAX,
        ..ScanThresholds::default()
    };
    let report = assess_supervisor_scan(&window, ratio_only, now());

    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(
        report.detail.contains("budget 5.0%"),
        "the raise must be attributed to the ratio: {}",
        report.detail
    );
}

// ---------------------------------------------------------------------------
// Clean windows must not raise — otherwise Degraded is the only value.
// ---------------------------------------------------------------------------

#[test]
fn control_a_clean_window_reads_served_and_does_not_raise() {
    let lines: Vec<String> = (0..200).map(|_| SIGHTED_LINE.to_owned()).collect();
    let report = assess(&parse_scan_window(&borrowed(&lines)));

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert!(!report.verdict.is_raise());
    assert_eq!(report.blind_cycles, 0);
    assert_eq!(report.blind_ratio, Some(0.0));
    assert_eq!(report.boundary, None);
    assert!(report.detail.contains("none blind"), "{}", report.detail);
}

#[test]
fn a_few_scattered_blind_cycles_stay_inside_the_budget() {
    let mut lines: Vec<String> = Vec::new();
    for index in 0..200 {
        if index % 100 == 0 {
            lines.push(blind_line(1));
        } else {
            lines.push(SIGHTED_LINE.to_owned());
        }
    }
    let report = assess(&parse_scan_window(&borrowed(&lines)));

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert_eq!(report.blind_cycles, 2);
    assert_eq!(report.max_consecutive_blind, 1);
}

// ---------------------------------------------------------------------------
// Consecutive blindness is a second, independent signal. A burst is worse than
// the same count scattered, and a long window's ratio would dilute it away.
// ---------------------------------------------------------------------------

#[test]
fn a_burst_of_consecutive_blind_cycles_raises_even_when_the_ratio_does_not() {
    let mut lines: Vec<String> = (0..400).map(|_| SIGHTED_LINE.to_owned()).collect();
    for consecutive in 1..=9 {
        lines.push(blind_line(consecutive));
    }
    lines.extend((0..400).map(|_| SIGHTED_LINE.to_owned()));

    let window = parse_scan_window(&borrowed(&lines));
    let ratio = window.blind_ratio().expect("cycles present");
    assert!(
        ratio < ScanThresholds::default().max_blind_ratio,
        "the fixture must sit UNDER the ratio budget so only the run can raise, got {ratio}"
    );

    let report = assess(&window);
    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert_eq!(report.max_consecutive_blind, 9);
    assert!(
        report.detail.contains("longest blind run 9"),
        "{}",
        report.detail
    );
}

/// Planted control for the burst test: the same nine blind cycles, spread out
/// so no run exceeds the budget, must NOT raise on the run signal.
#[test]
fn control_the_same_nine_blind_cycles_scattered_do_not_raise() {
    let mut lines: Vec<String> = Vec::new();
    for index in 0..810 {
        if index % 90 == 0 {
            lines.push(blind_line(1));
        } else {
            lines.push(SIGHTED_LINE.to_owned());
        }
    }
    let window = parse_scan_window(&borrowed(&lines));
    assert_eq!(window.blind_cycles, 9);
    assert_eq!(window.max_consecutive_blind, 1);

    let report = assess(&window);
    assert_eq!(
        report.verdict,
        ServiceVerdict::Served,
        "nine scattered blind cycles in 810 must stay under both budgets: {}",
        report.detail
    );
}

#[test]
fn the_supervisors_own_counter_raises_at_its_self_restart_ceiling() {
    // Only three adjacent blind cycles — under the run budget — but the printed
    // counter says the supervisor has already reached 9/9.
    let lines = vec![
        SIGHTED_LINE.to_owned(),
        blind_line(7),
        blind_line(8),
        blind_line(9),
        SIGHTED_LINE.to_owned(),
    ];
    let mut padded = lines;
    padded.extend((0..200).map(|_| SIGHTED_LINE.to_owned()));

    let window = parse_scan_window(&borrowed(&padded));
    assert_eq!(window.reported_consecutive_blind, Some(9));
    assert_eq!(window.consecutive_ceiling, Some(9));
    assert!(
        window.max_consecutive_blind <= ScanThresholds::default().max_consecutive_blind,
        "the observed run must stay under budget so the printed counter is the only signal"
    );

    let report = assess(&window);
    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(
        report.detail.contains("self-restart ceiling"),
        "{}",
        report.detail
    );
}

/// The trap the counter parser exists for: the same line carries
/// `running_macos_vms=1/2`, and reading that as the blind counter would report
/// a supervisor at 1 of 2 instead of 6 of 9.
#[test]
fn the_blind_counter_is_not_confused_by_the_vm_census_fraction() {
    let line = blind_line(6);
    let window = parse_scan_window(&[line.as_str()]);
    assert_eq!(window.reported_consecutive_blind, Some(6));
    assert_eq!(window.consecutive_ceiling, Some(9));
}

/// The planted control for the parser above, and the one that actually
/// exercises the guard: a blind line that carries **no** counter must report
/// none. Without the guard the VM census `1/2` is the only fraction left on the
/// line, so the supervisor reads as 1 blind cycle of a ceiling of 2 — a fully
/// fabricated pair, invented out of a field about virtual machines.
#[test]
fn control_a_blind_line_without_a_counter_reports_no_counter() {
    let window = parse_scan_window(&["SCAN BLIND — NOT idling as empty (running_macos_vms=1/2)"]);

    assert_eq!(window.blind_cycles, 1);
    assert_eq!(window.reported_consecutive_blind, None);
    assert_eq!(window.consecutive_ceiling, None);
}

// ---------------------------------------------------------------------------
// Naming the right subsystem: a timeout is transport, not authentication.
// ---------------------------------------------------------------------------

#[test]
fn a_scan_timeout_classifies_as_transport() {
    assert_eq!(
        classify_scan_failure(&blind_line(6)),
        Some(Boundary::Transport)
    );
    assert_eq!(
        classify_scan_failure(BLIND_RESTART_LINE),
        Some(Boundary::Transport)
    );
}

/// The planted control for the classifier, and the assertion the incident is
/// really about: the supervisor's own line says "fresh gh auth", and it must
/// still NOT read as an authentication or permission fault. Auth was never
/// broken; the restart it triggered could not have helped.
#[test]
fn control_a_timeout_does_not_read_as_an_auth_or_permission_fault() {
    let boundary = classify_scan_failure(BLIND_RESTART_LINE).expect("a failure line");

    assert_ne!(boundary, Boundary::Permission);
    assert_ne!(boundary, Boundary::Identity);
    assert!(
        BLIND_RESTART_LINE.contains("gh auth"),
        "the fixture must actually contain the misleading word, or this proves nothing"
    );
    assert!(boundary.equivalent_path_may_exist());
    assert!(
        boundary.next_action().contains("do not re-authenticate"),
        "the remedy must contradict the supervisor's own: {}",
        boundary.next_action()
    );

    let window = parse_scan_window(&[BLIND_RESTART_LINE]);
    assert_eq!(window.observed_boundary, Some(Boundary::Transport));
    assert_eq!(assess(&window).observed_boundary, Some(Boundary::Transport));
}

#[test]
fn genuine_credential_evidence_still_classifies_as_permission() {
    assert_eq!(
        classify_scan_failure("SCAN BLIND (gh queue scan failed) 2/9 — HTTP 401 Bad credentials"),
        Some(Boundary::Permission)
    );
}

#[test]
fn a_rate_limit_is_transport_not_permission_despite_the_403() {
    assert_eq!(
        classify_scan_failure("gh queue scan failed: HTTP 403 secondary rate limit exceeded"),
        Some(Boundary::Transport)
    );
}

#[test]
fn a_healthy_cycle_is_not_classified_as_a_failure() {
    assert_eq!(classify_scan_failure(SIGHTED_LINE), None);
    assert_eq!(classify_scan_failure(CHATTER_LINE), None);
}

#[test]
fn credential_evidence_outranks_timeouts_in_a_mixed_window() {
    let lines = vec![
        blind_line(1),
        blind_line(2),
        "SCAN BLIND (gh queue scan failed) 3/9 — Bad credentials".to_owned(),
        blind_line(4),
    ];
    let window = parse_scan_window(&borrowed(&lines));
    assert_eq!(window.observed_boundary, Some(Boundary::Permission));
}

// ---------------------------------------------------------------------------
// A budget set by absence. Same latencies, opposite verdicts.
// ---------------------------------------------------------------------------

/// Latencies the proxy actually imposes. Comfortable under 180s, fatal under
/// the 15s a lane inherits when it declares nothing.
const OBSERVED_LATENCIES: [i64; 4] = [12, 22, 31, 18];

#[test]
fn a_lane_that_declares_no_scan_timeout_goes_degraded_on_the_inherited_default() {
    let report = assess_scan_budget(
        "tartci-macos-release",
        None,
        &OBSERVED_LATENCIES,
        ScanThresholds::default(),
    );

    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert!(report.verdict.is_raise());
    assert!(report.inherits_default);
    assert_eq!(report.effective_budget_secs, DEFAULT_SCAN_TIMEOUT_SECS);
    assert_eq!(report.worst_latency_secs, Some(31));
    assert_eq!(report.over_budget_samples, 3);
    assert!(
        report.detail.contains("inherited by omission"),
        "the message must name the omission as the cause: {}",
        report.detail
    );
    assert!(
        report.detail.contains("do not re-authenticate"),
        "{}",
        report.detail
    );
}

/// The planted control, and the asymmetry the incident turned on: identical
/// input, identical thresholds, only the declared budget differs — and the
/// verdict inverts. If this test went red the previous one would be measuring
/// the latencies rather than the budget.
#[test]
fn control_the_same_latencies_under_a_declared_180s_budget_read_served() {
    let report = assess_scan_budget(
        "tartci-macos-gate",
        Some(180),
        &OBSERVED_LATENCIES,
        ScanThresholds::default(),
    );

    assert_eq!(
        report.verdict,
        ServiceVerdict::Served,
        "the same seconds must be comfortable under the gate lane's declared budget: {}",
        report.detail
    );
    assert!(!report.verdict.is_raise());
    assert!(!report.inherits_default);
    assert_eq!(report.effective_budget_secs, 180);
    assert_eq!(report.over_budget_samples, 0);
    assert_eq!(report.worst_latency_secs, Some(31));
}

#[test]
fn budget_pressure_raises_before_anything_has_actually_failed() {
    let report = assess_scan_budget(
        "tartci-macos-gate",
        Some(180),
        &[150, 90],
        ScanThresholds::default(),
    );

    assert_eq!(report.verdict, ServiceVerdict::Degraded);
    assert_eq!(report.over_budget_samples, 0);
    assert!(
        report.detail.contains("Nothing has failed yet"),
        "{}",
        report.detail
    );
}

#[test]
fn the_budget_ratio_is_reported_rather_than_collapsed_to_pass_or_fail() {
    let inherited = assess_scan_budget("release", None, &[31], ScanThresholds::default());
    let declared = assess_scan_budget("gate", Some(180), &[31], ScanThresholds::default());

    let inherited_ratio = inherited.worst_budget_ratio.expect("ratio");
    let declared_ratio = declared.worst_budget_ratio.expect("ratio");
    assert!((inherited_ratio - 31.0 / 15.0).abs() < 1e-9);
    assert!((declared_ratio - 31.0 / 180.0).abs() < 1e-9);
    assert!(inherited_ratio > declared_ratio);
}

#[test]
fn a_window_that_stated_no_latency_is_unknown_not_a_passing_budget() {
    let report = assess_scan_budget("release", None, &[], ScanThresholds::default());

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert!(report.verdict.is_raise());
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert_eq!(report.worst_budget_ratio, None);
    assert!(
        report.detail.contains("not a budget that was met"),
        "{}",
        report.detail
    );
}

#[test]
fn a_non_positive_declared_budget_is_unknown_rather_than_a_division() {
    let report = assess_scan_budget("broken", Some(0), &[12], ScanThresholds::default());

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
}

// ---------------------------------------------------------------------------
// Latency extraction: a marker is required, because the healthy line already
// contains a number of seconds that is not a latency.
// ---------------------------------------------------------------------------

#[test]
fn a_blind_lines_stated_duration_is_read_as_a_latency_sample() {
    let window = parse_scan_window(&[BLIND_RESTART_LINE]);
    assert_eq!(window.observed_latencies_secs, vec![180]);
}

/// Control for the extractor: the healthy cycle line says `yielding 20s`, which
/// is a sleep the supervisor chose, not a cost it paid. Reading it as a latency
/// would report the healthiest lines as the slowest.
#[test]
fn control_the_yield_duration_on_a_healthy_line_is_not_a_latency() {
    let window = parse_scan_window(&[SIGHTED_LINE]);
    assert_eq!(window.sighted_cycles, 1);
    assert!(
        window.observed_latencies_secs.is_empty(),
        "`yielding 20s` must not be mistaken for a scan latency: {:?}",
        window.observed_latencies_secs
    );
}

#[test]
fn an_explicit_scan_marker_is_read_as_a_latency() {
    let window = parse_scan_window(&["• yielding 20s (queued=0 scan=9s)"]);
    assert_eq!(window.observed_latencies_secs, vec![9]);
}

// ---------------------------------------------------------------------------
// An instrument that read nothing has not observed health.
// ---------------------------------------------------------------------------

#[test]
fn an_empty_window_is_unknown_with_a_named_boundary() {
    let report = assess(&parse_scan_window(&[]));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert!(report.verdict.is_raise());
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert_eq!(report.blind_ratio, None);
    assert!(
        report.detail.contains("nothing was asserted"),
        "{}",
        report.detail
    );
}

#[test]
fn a_window_of_unrecognisable_lines_is_unknown_not_a_pass() {
    let lines = vec![CHATTER_LINE; 500];
    let report = assess(&parse_scan_window(&lines));

    assert_eq!(report.verdict, ServiceVerdict::Unknown);
    assert_eq!(report.boundary, Some(Boundary::Parse));
    assert_eq!(report.lines_scanned, 500);
    assert_eq!(report.sighted_cycles, 0);
    assert_eq!(report.blind_cycles, 0);
}

/// Planted control for the two tests above: the identical assertion over a
/// window that *does* contain cycles must produce a real verdict, proving the
/// `Unknown` came from the input and not from an assertion that always says so.
#[test]
fn control_a_window_with_one_recognisable_cycle_produces_a_real_verdict() {
    let mut lines = vec![CHATTER_LINE.to_owned(); 499];
    lines.push(SIGHTED_LINE.to_owned());
    let report = assess(&parse_scan_window(&borrowed(&lines)));

    assert_eq!(report.verdict, ServiceVerdict::Served);
    assert_eq!(report.boundary, None);
    assert_eq!(
        report.lines_scanned - report.blind_cycles - report.sighted_cycles,
        499
    );
}

// ---------------------------------------------------------------------------
// Chatter between two blind scans is not a successful scan between them.
// ---------------------------------------------------------------------------

#[test]
fn unrelated_lines_do_not_break_a_run_of_blind_cycles() {
    let lines = vec![
        blind_line(1),
        CHATTER_LINE.to_owned(),
        blind_line(2),
        CHATTER_LINE.to_owned(),
        blind_line(3),
    ];
    let window = parse_scan_window(&borrowed(&lines));

    assert_eq!(window.max_consecutive_blind, 3);
    assert_eq!(window.trailing_consecutive_blind, 3);
    assert_eq!(window.unrecognized_lines, 2);
}

/// Control for the rule above: a *sighted* cycle between two blind ones does
/// break the run, because that one really is a scan that succeeded.
#[test]
fn control_a_sighted_cycle_does_break_a_run_of_blind_cycles() {
    let lines = vec![
        blind_line(1),
        SIGHTED_LINE.to_owned(),
        blind_line(1),
        SIGHTED_LINE.to_owned(),
        blind_line(1),
    ];
    let window = parse_scan_window(&borrowed(&lines));

    assert_eq!(window.max_consecutive_blind, 1);
    assert_eq!(window.blind_cycles, 3);
    assert_eq!(window.sighted_cycles, 2);
}

// ---------------------------------------------------------------------------
// Roll-up compatibility with the sibling service taxonomy.
// ---------------------------------------------------------------------------

#[test]
fn supervisor_verdicts_order_against_the_shared_service_taxonomy() {
    let clean_lines: Vec<String> = (0..50).map(|_| SIGHTED_LINE.to_owned()).collect();
    let clean = assess(&parse_scan_window(&borrowed(&clean_lines)));
    let incident_lines = release_supervisor_window();
    let incident = assess(&parse_scan_window(&borrowed(&incident_lines)));

    assert!(incident.verdict > clean.verdict);
    assert!(ServiceVerdict::Unknown > incident.verdict);
}
