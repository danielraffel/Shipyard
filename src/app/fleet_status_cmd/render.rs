use std::collections::BTreeMap;
use std::io::Write;

use serde_json::Value;

use super::assessment::{
    FleetAssessment, HostFleetStatus, MergeQueueProbe, QueuedSummary, ReleaseProbe,
};
use crate::app::CliFailure;
use crate::output::write_json_envelope;

pub(in crate::app) fn render_fleet_assessment<W: Write>(
    assessment: &FleetAssessment,
    json: bool,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    if json {
        write_fleet_json(stdout, "runner.fleet-status", None, assessment)
    } else {
        write_fleet_text(stdout, assessment)
    }
}

pub(in crate::app) fn render_fleet_watch_event<W: Write>(
    assessment: &FleetAssessment,
    stdout: &mut W,
) -> Result<(), CliFailure> {
    write_fleet_json(
        stdout,
        "runner.watch",
        Some(("event", Value::from("fleet_liveness"))),
        assessment,
    )
}

fn write_fleet_json<W: Write>(
    stdout: &mut W,
    command: &str,
    extra: Option<(&str, Value)>,
    view: &FleetAssessment,
) -> Result<(), CliFailure> {
    let mut data = BTreeMap::new();
    if let Some((key, value)) = extra {
        data.insert(key.to_owned(), value);
    }
    data.insert("repo".to_owned(), Value::from(view.repo.as_str()));
    data.insert("target".to_owned(), Value::from(view.target.as_str()));
    data.insert("free_slots".to_owned(), Value::from(view.free));
    data.insert(
        "routable_free_slots".to_owned(),
        Value::from(view.routable_free_slots),
    );
    data.insert(
        "any_unreadable".to_owned(),
        Value::from(
            view.capacity_unreadable
                || view.doctor_unreadable
                || !view.queue.readable
                || view.observation_incomplete,
        ),
    );
    data.insert(
        "supervisor_unhealthy".to_owned(),
        Value::from(view.supervisor_unhealthy),
    );
    data.insert("problem_hosts".to_owned(), Value::from(view.problem_hosts));
    data.insert(
        "queued_age_threshold_secs".to_owned(),
        Value::from(view.queued_age_threshold_secs),
    );
    data.insert(
        "queue_run_limit".to_owned(),
        Value::from(view.queue_run_limit),
    );
    data.insert(
        "queued_age_with_capacity".to_owned(),
        Value::from(view.queued_age_with_capacity),
    );
    data.insert("queue".to_owned(), queue_to_json(&view.queue));
    data.insert("base".to_owned(), Value::from(view.base.as_str()));
    data.insert(
        "merge_queue_stall_threshold_secs".to_owned(),
        Value::from(view.merge_queue_stall_threshold_secs),
    );
    data.insert(
        "merge_queue".to_owned(),
        merge_queue_to_json(&view.merge_queue),
    );
    data.insert(
        "release_stale_threshold_secs".to_owned(),
        Value::from(view.release_stale_threshold_secs),
    );
    data.insert("release".to_owned(), release_to_json(&view.release));
    data.insert(
        "observation_reason_codes".to_owned(),
        serde_json::to_value(&view.observation_reason_codes)
            .expect("observation reasons serialize"),
    );
    data.insert(
        "hosts".to_owned(),
        Value::from(view.hosts.iter().map(host_to_json).collect::<Vec<_>>()),
    );
    data.insert(
        "runners".to_owned(),
        serde_json::json!({
            "readable": view.runners.readable,
            "source": view.runners.source,
            "total": view.runners.runners.len(),
            "online": view.runners.runners.iter().filter(|runner| runner.status.eq_ignore_ascii_case("online")).count(),
            "idle": view.runners.runners.iter().filter(|runner| runner.status.eq_ignore_ascii_case("online") && !runner.busy).count(),
            "offline": view.runners.runners.iter().filter(|runner| runner.status.eq_ignore_ascii_case("offline")).count(),
            "registrations": view.runners.runners,
        }),
    );
    data.insert(
        "routing_mismatches".to_owned(),
        serde_json::to_value(&view.routing_mismatches).expect("routing mismatches serialize"),
    );
    data.insert(
        "expected_hosts".to_owned(),
        serde_json::to_value(&view.expected_hosts).expect("expected hosts serialize"),
    );
    write_json_envelope(stdout, command, data)
        .map_err(|e| CliFailure::new(1, format!("failed to write JSON: {e}")))
}

fn write_fleet_text<W: Write>(stdout: &mut W, view: &FleetAssessment) -> Result<(), CliFailure> {
    writeln!(
        stdout,
        "fleet-status repo={repo} target={} free={free} routable_free={routable_free_slots}",
        view.target,
        repo = view.repo,
        free = view.free,
        routable_free_slots = view.routable_free_slots
    )
    .map_err(text_write_failure)?;
    for host in &view.hosts {
        let running = host
            .capacity
            .running
            .map_or_else(|| "?".to_owned(), |value| value.to_string());
        writeln!(
            stdout,
            "  {:<10} cap={} running={} free={} routable={} supervisors={}/{} stale={} problems={} source={}",
            host.capacity.class,
            host.capacity.cap,
            running,
            host.capacity.free(),
            host.routable,
            host.fresh_supervisor_count,
            host.supervisor_count,
            host.stale_supervisor_count,
            host.problem_count,
            host.doctor.source
        )
        .map_err(text_write_failure)?;
        if !host.storage_problems.is_empty() {
            for problem in &host.storage_problems {
                writeln!(stdout, "    storage: {problem}").map_err(text_write_failure)?;
            }
        }
    }
    writeln!(
        stdout,
        "  runners: total={} online={} idle={} offline={} readable={}",
        view.runners.runners.len(),
        view.runners
            .runners
            .iter()
            .filter(|runner| runner.status.eq_ignore_ascii_case("online"))
            .count(),
        view.runners
            .runners
            .iter()
            .filter(|runner| runner.status.eq_ignore_ascii_case("online") && !runner.busy)
            .count(),
        view.runners
            .runners
            .iter()
            .filter(|runner| runner.status.eq_ignore_ascii_case("offline"))
            .count(),
        view.runners.readable
    )
    .map_err(text_write_failure)?;
    for mismatch in &view.routing_mismatches {
        writeln!(
            stdout,
            "    routing mismatch: run={} job={} candidates={} reason={}",
            mismatch.run_id,
            mismatch.job,
            mismatch.idle_candidates.join(","),
            mismatch.reason
        )
        .map_err(text_write_failure)?;
    }
    write_expected_hosts_text(stdout, view)?;
    writeln!(
        stdout,
        "  queued macOS: count={} oldest_age_secs={} threshold={} readable={}",
        view.queue.count,
        view.queue
            .oldest_age_secs
            .map_or_else(|| "-".to_owned(), |age| age.to_string()),
        view.queued_age_threshold_secs,
        view.queue.readable
    )
    .map_err(text_write_failure)?;
    write_merge_queue_text(stdout, view)?;
    write_release_text(stdout, view)?;
    if view.should_fail {
        writeln!(
            stdout,
            "fleet-status: attention required (see fields above)"
        )
        .map_err(text_write_failure)?;
    }
    Ok(())
}

fn write_expected_hosts_text<W: Write>(
    stdout: &mut W,
    view: &FleetAssessment,
) -> Result<(), CliFailure> {
    for host in &view.expected_hosts {
        writeln!(
            stdout,
            "  expected host: name={} active={} online={}/{} idle={} runners={} problem={}",
            host.name,
            host.active,
            host.online,
            host.min_online,
            host.idle,
            host.matching_runners.join(","),
            host.problem.as_deref().unwrap_or("-")
        )
        .map_err(text_write_failure)?;
    }
    Ok(())
}

fn host_to_json(host: &HostFleetStatus) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("class".to_owned(), Value::from(host.capacity.class.clone()));
    m.insert(
        "ssh".to_owned(),
        host.capacity.ssh.clone().map_or(Value::Null, Value::from),
    );
    m.insert("cap".to_owned(), Value::from(host.capacity.cap));
    m.insert(
        "running".to_owned(),
        host.capacity.running.map_or(Value::Null, Value::from),
    );
    m.insert("free".to_owned(), Value::from(host.capacity.free()));
    m.insert(
        "capacity_readable".to_owned(),
        Value::from(host.capacity.readable()),
    );
    m.insert(
        "doctor_readable".to_owned(),
        Value::from(host.doctor.readable),
    );
    m.insert("source".to_owned(), Value::from(host.doctor.source.clone()));
    m.insert("routable".to_owned(), Value::from(host.routable));
    m.insert(
        "supervisor_count".to_owned(),
        Value::from(host.supervisor_count),
    );
    m.insert(
        "fresh_supervisor_count".to_owned(),
        Value::from(host.fresh_supervisor_count),
    );
    m.insert(
        "stale_supervisor_count".to_owned(),
        Value::from(host.stale_supervisor_count),
    );
    m.insert("problem_count".to_owned(), Value::from(host.problem_count));
    m.insert(
        "storage".to_owned(),
        serde_json::to_value(&host.storage).expect("storage probe serializes"),
    );
    m.insert(
        "storage_problems".to_owned(),
        Value::from(host.storage_problems.clone()),
    );
    m.insert(
        "github_runner_count".to_owned(),
        Value::from(host.github_runner_count),
    );
    m.insert(
        "stale_vm_count".to_owned(),
        Value::from(host.stale_vm_count),
    );
    if host.doctor.digest.is_some() {
        m.insert("problems".to_owned(), Value::from(host.problems.clone()));
        m.insert(
            "supervisors".to_owned(),
            Value::from(host.supervisors.clone()),
        );
    }
    Value::Object(m)
}

fn queue_to_json(queue: &QueuedSummary) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("readable".to_owned(), Value::from(queue.readable));
    m.insert("source".to_owned(), Value::from(queue.source.clone()));
    m.insert("count".to_owned(), Value::from(queue.count));
    m.insert(
        "oldest_age_secs".to_owned(),
        queue.oldest_age_secs.map_or(Value::Null, Value::from),
    );
    Value::Object(m)
}

fn merge_queue_to_json(probe: &MergeQueueProbe) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("readable".to_owned(), Value::from(probe.readable));
    m.insert("source".to_owned(), Value::from(probe.source.clone()));
    m.insert(
        "observation_reason_codes".to_owned(),
        serde_json::to_value(&probe.reason_codes).expect("merge observation reasons serialize"),
    );
    m.insert(
        "report".to_owned(),
        probe.report.as_ref().map_or(Value::Null, |report| {
            serde_json::to_value(report).expect("merge-queue liveness serialization")
        }),
    );
    Value::Object(m)
}

fn release_to_json(probe: &ReleaseProbe) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("readable".to_owned(), Value::from(probe.readable));
    m.insert("source".to_owned(), Value::from(probe.source.clone()));
    m.insert(
        "observation_reason_codes".to_owned(),
        serde_json::to_value(&probe.reason_codes).expect("release observation reasons serialize"),
    );
    m.insert(
        "report".to_owned(),
        probe.report.as_ref().map_or(Value::Null, |report| {
            serde_json::to_value(report).expect("release liveness serialization")
        }),
    );
    Value::Object(m)
}

fn write_merge_queue_text<W: Write>(
    stdout: &mut W,
    view: &FleetAssessment,
) -> Result<(), CliFailure> {
    let Some(report) = &view.merge_queue.report else {
        writeln!(
            stdout,
            "  merge queue {}: unreadable ({})",
            view.base, view.merge_queue.source
        )
        .map_err(text_write_failure)?;
        return Ok(());
    };
    let front = report
        .front
        .as_ref()
        .map_or_else(|| "-".to_owned(), |front| format!("#{}", front.pr));
    writeln!(
        stdout,
        "  merge queue {}: front={} required_materialized={} required_progressed={} stalled_with_idle_capacity={} threshold={} readable={}",
        view.base,
        front,
        report.materialized_required_checks,
        report.progressed_required_checks,
        report.front_stalled_with_idle_capacity,
        view.merge_queue_stall_threshold_secs,
        view.merge_queue.readable
    )
    .map_err(text_write_failure)?;
    if !view.merge_queue.reason_codes.is_empty() {
        writeln!(
            stdout,
            "    observation_reasons={:?}",
            view.merge_queue.reason_codes
        )
        .map_err(text_write_failure)?;
    }
    if !report.enrollment_cleared_prs.is_empty() {
        let prs = report
            .enrollment_cleared_prs
            .iter()
            .map(|pr| format!("#{pr}"))
            .collect::<Vec<_>>()
            .join(",");
        writeln!(stdout, "    auto_merge_enrollment_cleared={prs}").map_err(text_write_failure)?;
    }
    for occupier in &report.capacity_occupiers {
        writeln!(
            stdout,
            "    {:?}: run={} pr={} job={} runner={} url={}",
            occupier.kind,
            occupier.run_id,
            occupier
                .pr
                .map_or_else(|| "-".to_owned(), |pr| format!("#{pr}")),
            occupier.job,
            occupier.runner_name,
            occupier.url.as_deref().unwrap_or("-")
        )
        .map_err(text_write_failure)?;
    }
    Ok(())
}

fn write_release_text<W: Write>(stdout: &mut W, view: &FleetAssessment) -> Result<(), CliFailure> {
    let Some(report) = &view.release.report else {
        writeln!(stdout, "  release: unavailable ({})", view.release.source)
            .map_err(text_write_failure)?;
        return Ok(());
    };
    writeln!(
        stdout,
        "  release: tag={} releasable_age_secs={} commits_ahead={} stale={} threshold={} readable={}",
        report.tag,
        report.age_secs,
        report.commits_ahead,
        report.stale_with_unreleased_commits,
        view.release_stale_threshold_secs,
        view.release.readable
    )
    .map_err(text_write_failure)?;
    Ok(())
}

fn text_write_failure(error: impl std::fmt::Display) -> CliFailure {
    CliFailure::new(1, format!("failed to write fleet status: {error}"))
}
