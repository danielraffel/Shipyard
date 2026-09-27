use chrono::{Duration, TimeZone};

use super::*;

fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 1, 0, 0, 0)
        .single()
        .expect("fixture time")
}

struct Job {
    pr: i64,
    status: &'static str,
    queued_s: i64,
    wait_s: i64,
    minutes: i64,
    runner_assigned: bool,
}

fn sample(job: &Job) -> ProxySample {
    let queued = t0() + Duration::seconds(job.queued_s);
    let started = queued + Duration::seconds(job.wait_s);
    ProxySample {
        lane: "macos".to_owned(),
        status: job.status.to_owned(),
        pr: Some(("o/r".to_owned(), job.pr)),
        queued_at: Some(queued),
        started_at: job.runner_assigned.then_some(started),
        completed_at: Some(started + Duration::minutes(job.minutes)),
        total_ms: Some(job.minutes * 60_000),
        runner_assigned: Some(job.runner_assigned),
        cache_hit: None,
    }
}

/// Quiet 1am-style window: nothing is ahead in the queue, jobs are fast, but
/// every PR needs two attempts and 30% of attempts fail.
fn quiet_but_wasteful() -> Vec<ProxySample> {
    (0..20)
        .map(|index| Job {
            pr: index / 2,
            status: if index % 10 < 3 { "failure" } else { "success" },
            queued_s: index * 10_000,
            wait_s: 60,
            minutes: 10,
            runner_assigned: true,
        })
        .map(|job| sample(&job))
        .collect()
}

/// Peak-load window: a 20-deep burst queued together, every job twice as slow
/// on a busy host, but one clean attempt per PR and far less wait per job
/// ahead.
fn loaded_but_efficient() -> Vec<ProxySample> {
    (0..20)
        .map(|index| Job {
            pr: 100 + index,
            status: "success",
            queued_s: index,
            wait_s: 100,
            minutes: 20,
            runner_assigned: true,
        })
        .map(|job| sample(&job))
        .collect()
}

fn refs(samples: &[ProxySample]) -> Vec<&ProxySample> {
    samples.iter().collect()
}

fn delta<'a>(comparison: &'a Comparison, name: &str) -> &'a ProxyDelta {
    comparison
        .proxies
        .iter()
        .find(|delta| delta.name == name)
        .expect("proxy present")
}

#[test]
fn load_only_slowdown_with_better_proxies_is_an_improvement() {
    let before = quiet_but_wasteful();
    let after = loaded_but_efficient();
    let comparison = compare(&refs(&before), &refs(&after), Basis::Proxy);

    assert_eq!(comparison.wall_time_verdict, Verdict::Regressed);
    assert_eq!(comparison.proxy_verdict, Verdict::Improved);
    assert_eq!(comparison.verdict, Verdict::Improved);
    assert_eq!(
        delta(&comparison, "failure_share").direction,
        Direction::Improved
    );
    assert_eq!(delta(&comparison, "attempts_per_pr").before, Some(2.0));
    assert_eq!(delta(&comparison, "attempts_per_pr").after, Some(1.0));
    let wait = delta(&comparison, "queue_wait_per_job_ahead_ms");
    assert_eq!(wait.before, Some(60_000.0));
    assert!(wait.after.expect("measured") < 10_000.0, "{wait:?}");
    assert_eq!(wait.direction, Direction::Improved);
    assert_eq!(comparison.context.label, WALL_CONTEXT_LABEL);
    assert_eq!(comparison.context.before_p50_ms, Some(600_000));
    assert_eq!(comparison.context.after_p50_ms, Some(1_200_000));
}

#[test]
fn wall_time_basis_restores_the_duration_verdict() {
    let before = quiet_but_wasteful();
    let after = loaded_but_efficient();
    let comparison = compare(&refs(&before), &refs(&after), Basis::WallTime);
    assert_eq!(comparison.verdict, Verdict::Regressed);
    assert_eq!(comparison.proxy_verdict, Verdict::Improved);
    assert_eq!(comparison.basis, Basis::WallTime);
}

#[test]
fn quiet_speedup_with_worse_proxies_is_a_regression() {
    let before = loaded_but_efficient();
    let mut after = quiet_but_wasteful();
    for (index, sample) in after.iter_mut().enumerate().take(6) {
        if index % 2 == 0 {
            sample.status = "cancelled".to_owned();
            sample.runner_assigned = Some(false);
            sample.started_at = None;
        }
    }
    let comparison = compare(&refs(&before), &refs(&after), Basis::Proxy);

    assert_eq!(comparison.wall_time_verdict, Verdict::Improved);
    assert_eq!(comparison.proxy_verdict, Verdict::Regressed);
    assert_eq!(comparison.verdict, Verdict::Regressed);
    assert_eq!(
        delta(&comparison, "attempts_per_pr").direction,
        Direction::Regressed
    );
    assert_eq!(
        delta(&comparison, "starvation_share").after,
        Some(3.0 / 20.0)
    );
}

#[test]
fn a_small_window_says_insufficient_sample_not_a_verdict() {
    let before = quiet_but_wasteful();
    let after = loaded_but_efficient();
    let comparison = compare(&refs(&before[..4]), &refs(&after[..4]), Basis::Proxy);
    assert_eq!(comparison.verdict, Verdict::InsufficientSample);
    assert!(
        comparison
            .proxies
            .iter()
            .all(|delta| delta.direction == Direction::InsufficientSample)
    );
    assert!(comparison.summary().starts_with("insufficient_sample"));
}

#[test]
fn identical_windows_are_unchanged() {
    let before = quiet_but_wasteful();
    let comparison = compare(&refs(&before), &refs(&before), Basis::Proxy);
    assert_eq!(comparison.verdict, Verdict::Unchanged);
    assert_eq!(comparison.wall_time_verdict, Verdict::Unchanged);
}

#[test]
fn unknown_runner_assignment_is_excluded_not_counted_as_starved() {
    let mut samples = quiet_but_wasteful();
    for sample in &mut samples {
        sample.runner_assigned = None;
        sample.status = "cancelled".to_owned();
    }
    let values = measure(&refs(&samples));
    let starvation = values
        .iter()
        .find(|value| value.name == "starvation_share")
        .expect("starvation measured");
    assert_eq!(starvation.sample, 0);
    assert_eq!(starvation.value, None);
    assert!(!starvation.sufficient);
}

#[test]
fn wait_per_job_ahead_counts_only_jobs_still_waiting() {
    let first = sample(&Job {
        pr: 1,
        status: "success",
        queued_s: 0,
        wait_s: 10,
        minutes: 1,
        runner_assigned: true,
    });
    // Queued at 5s, while `first` is still waiting (it starts at 10s).
    let second = sample(&Job {
        pr: 2,
        status: "success",
        queued_s: 5,
        wait_s: 20,
        minutes: 1,
        runner_assigned: true,
    });
    // Queued at 30s, after both started: nothing ahead.
    let third = sample(&Job {
        pr: 3,
        status: "success",
        queued_s: 30,
        wait_s: 8,
        minutes: 1,
        runner_assigned: true,
    });
    let waits = wait_per_job_ahead_ms(&[&first, &second, &third]);
    assert_eq!(waits, vec![10_000.0, 10_000.0, 8_000.0]);
}
