//! Fixture tests for the instruments agents read: `summary --group-by host`,
//! `advise` lane keying, and `watch` denominators and required/advisory split.

use chrono::{Duration, Utc};
use rusqlite::Connection;

use super::proxy::Basis;
use super::*;

const PR_ALTERNATE: &str = "(github.event_name == 'pull_request') && (needs.classify.result != 'success') && 'macos' || 'macos-pr-unused'";

fn github_job(
    id: i64,
    name: &str,
    conclusion: &str,
    runner: Option<&str>,
    minutes: i64,
) -> GitHubRunJob {
    let started = Utc::now() - Duration::hours(2) + Duration::minutes(id);
    GitHubRunJob {
        id,
        run_id: Some(1000 + id),
        run_attempt: Some(1),
        name: name.to_owned(),
        status: Some("completed".to_owned()),
        conclusion: Some(conclusion.to_owned()),
        runner_name: runner.map(str::to_owned),
        runner_group_name: None,
        labels: Some(vec![
            "self-hosted".to_owned(),
            "pulp-build-pr-head".to_owned(),
        ]),
        created_at: Some((started - Duration::minutes(1)).to_rfc3339()),
        started_at: Some(started.to_rfc3339()),
        completed_at: Some((started + Duration::minutes(minutes)).to_rfc3339()),
    }
}

fn import(store: &MetricsStore, job: &GitHubRunJob) {
    let input = github_job_to_record("Generous-Corp/pulp", Some("build.yml"), "pulp", None, job);
    store.record_terminal_observation(&input).expect("record");
}

/// A gate served by throwaway just-in-time runners, as `metrics import
/// github` records it: every `macos` job on its own runner name, plus the
/// skipped alternates GitHub reports under their unevaluated names.
fn jit_gate_store() -> (tempfile::TempDir, MetricsStore) {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = MetricsStore::open(temp.path()).expect("store");
    let jobs = [
        github_job(
            1,
            "macos",
            "success",
            Some("m5studio-pulp-gate-01-42746-21"),
            20,
        ),
        github_job(
            2,
            "macos",
            "success",
            Some("m5studio-pulp-gate-slot2-02-42765-3"),
            22,
        ),
        github_job(
            3,
            "macos",
            "success",
            Some("m5studio-pulp-gate-01-42746-22"),
            24,
        ),
        github_job(
            4,
            "macos",
            "success",
            Some("m5studio-pulp-gate-slot2-02-42765-4"),
            26,
        ),
        github_job(
            5,
            "macos",
            "success",
            Some("studio-pulp-gate-01-76663-1"),
            40,
        ),
        github_job(
            6,
            "macos",
            "cancelled",
            Some("studio-pulp-gate-01-76663-2"),
            3,
        ),
        // Superseded by a push: says nothing about the host's health.
        github_job(
            10,
            "macos",
            "cancelled",
            Some("m5studio-pulp-gate-01-42746-23"),
            2,
        ),
        github_job(7, PR_ALTERNATE, "skipped", None, 0),
        github_job(8, PR_ALTERNATE, "skipped", None, 0),
        github_job(9, PR_ALTERNATE, "skipped", None, 0),
    ];
    for job in &jobs {
        import(&store, job);
    }
    (temp, store)
}

#[test]
fn host_group_folds_runner_names_into_the_machine() {
    let repo = Some("Generous-Corp/pulp");
    assert_eq!(
        host_group("m5studio-pulp-gate-01-42746-25", &[], repo),
        "m5studio"
    );
    assert_eq!(host_group("m1-pulp-gate-slot2-02-13611-9", &[], repo), "m1");
    assert_eq!(
        host_group("studio-pulp-gate-01-76663-1", &[], repo),
        "studio"
    );
    assert_eq!(host_group("pulp-m1-02", &[], repo), "m1");
    assert_eq!(
        host_group("pulp-daniels-macbook-pro-01-59251-1", &[], repo),
        "daniels-macbook-pro"
    );
    assert_eq!(
        host_group("GitHub Actions 1000154393", &[], repo),
        "github-hosted"
    );
    assert_eq!(host_group("m3", &[], repo), "m3", "a host stays itself");
    let labels = vec!["self-hosted".to_owned(), "pulp-host-macpro".to_owned()];
    assert_eq!(host_group("pulp-linux-07", &labels, repo), "macpro");
    // The event-class routing label is not a host label.
    let routing = vec!["pulp-build-pr-head".to_owned()];
    assert_eq!(host_group("m1-pulp-gate-01-1-1", &routing, repo), "m1");
}

#[test]
fn summary_groups_ephemeral_runners_by_host_on_request() {
    let (_temp, store) = jit_gate_store();
    let macos = |rows: Vec<MetricsSummaryRow>| -> Vec<(String, usize)> {
        rows.into_iter()
            .filter(|row| row.target == "macos")
            .map(|row| (row.host, row.count))
            .collect()
    };
    let by_runner = macos(store.summary(Some("pulp")).expect("summary"));
    assert_eq!(
        by_runner.len(),
        7,
        "one row per throwaway runner: {by_runner:?}"
    );
    let by_host = macos(
        store
            .summary_grouped(Some("pulp"), SummaryGroupBy::Host)
            .expect("summary"),
    );
    assert_eq!(
        by_host,
        [("m5studio".to_owned(), 5), ("studio".to_owned(), 2)]
    );
}

#[test]
fn advise_accumulates_a_jit_gate_by_host_and_resolved_name() {
    let (_temp, store) = jit_gate_store();
    let findings = store.advise("pulp").expect("advise");
    let lanes: Vec<&str> = findings.iter().map(|item| item.lane.as_str()).collect();
    assert_eq!(
        lanes,
        ["macos"],
        "the skipped alternates join `macos`: {findings:?}"
    );
    let macos = &findings[0];
    assert_eq!(macos.signal, "preferred_lane", "{macos:?}");
    assert_eq!(macos.sample_count, 4);
    assert!(macos.message.contains("on m5studio"), "{}", macos.message);
}

#[test]
fn advise_names_an_unhealthy_gate_instead_of_calling_it_unsampled() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = MetricsStore::open(temp.path()).expect("store");
    for id in 0..10 {
        let conclusion = if id % 2 == 0 { "failure" } else { "success" };
        import(
            &store,
            &github_job(id, "macos", conclusion, Some("m1-pulp-gate-01-9-1"), 30),
        );
    }
    import(
        &store,
        &github_job(20, "lint", "success", Some("m1-pulp-gate-01-9-1"), 1),
    );
    let findings = store.advise("pulp").expect("advise");
    let macos = findings
        .iter()
        .find(|item| item.lane == "macos")
        .expect("macos finding");
    assert_eq!(macos.signal, "no_healthy_lane");
    assert_eq!(macos.sample_count, 10);
    assert!(macos.message.contains("m1 50% of 10"), "{}", macos.message);
    let lint = findings
        .iter()
        .find(|item| item.lane == "lint")
        .expect("lint finding");
    assert_eq!(lint.signal, "insufficient_healthy_samples");
    assert_eq!(lint.sample_count, 1);
}

fn record_gate(store: &MetricsStore, job: &str, target: &str, status: &str, days_ago: i64, n: i64) {
    for index in 0..n {
        let completed = Utc::now() - Duration::days(days_ago) + Duration::minutes(index);
        store
            .record(&MetricRecordInput {
                project: "p".to_owned(),
                repo: Some("o/r".to_owned()),
                job: job.to_owned(),
                target: Some(target.to_owned()),
                host: Some("m3".to_owned()),
                duration_ms: 60_000,
                status: status.to_owned(),
                started_at: Some(completed - Duration::minutes(1)),
                completed_at: Some(completed),
                ..MetricRecordInput::default()
            })
            .expect("record");
    }
}

/// Earlier window: 20 green `macos` jobs. Later window: 12 green, 8 red.
/// Linux starts failing in the later window. Every run is then marked red, the way an
/// advisory Linux leg turns a workflow run red while `macos` stays green.
fn gate_and_advisory_store() -> (tempfile::TempDir, MetricsStore) {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = MetricsStore::open(temp.path()).expect("store");
    record_gate(&store, "macos", "macos-gate/merge_group", "success", 10, 20);
    record_gate(&store, "macos", "macos-gate/merge_group", "success", 2, 12);
    record_gate(&store, "macos", "macos-gate/merge_group", "failure", 2, 8);
    record_gate(&store, "Linux (x64)", "Linux (x64)", "success", 10, 10);
    record_gate(&store, "Linux (x64)", "Linux (x64)", "failure", 2, 10);
    record_gate(&store, "Linux (x64)", "Linux (x64)", "success", 2, 10);
    let conn = Connection::open(store.path()).expect("db");
    conn.execute("UPDATE runs SET status = 'failure'", [])
        .expect("red runs");
    (temp, store)
}

#[test]
fn watch_failure_share_is_the_named_jobs_own_conclusion() {
    let (_temp, store) = gate_and_advisory_store();
    let findings = store
        .watch_with_required("p", 7, Basis::Proxy, &["macos".to_owned()])
        .expect("watch");
    let gate = findings
        .iter()
        .find(|item| item.lane == "macos-gate/merge_group")
        .expect("gate finding");
    let share = gate
        .comparison
        .as_ref()
        .and_then(|comparison| {
            comparison
                .proxies
                .iter()
                .find(|delta| delta.name == "failure_share")
        })
        .expect("failure_share");
    assert_eq!(
        share.before,
        Some(0.0),
        "every earlier run was red, every job green"
    );
    assert_eq!(share.after, Some(0.4));
    assert_eq!((share.before_sample, share.after_sample), (20, 20));
    assert!(share.unit.starts_with("jobs"));
    assert!(gate.message.contains("(n=20/20 jobs"), "{}", gate.message);
    let denominator = gate.denominator.as_ref().expect("denominator");
    assert_eq!(denominator.unit, "jobs");
    assert_eq!(
        (denominator.previous_decided, denominator.current_decided),
        (20, 20)
    );
    assert_eq!(denominator.job_names, ["macos"]);
}

#[test]
fn watch_splits_required_gates_from_advisory_jobs() {
    let (_temp, store) = gate_and_advisory_store();
    let findings = store
        .watch_with_required("p", 7, Basis::Proxy, &["macos".to_owned()])
        .expect("watch");
    let classes: Vec<(&str, Option<GateClass>)> = findings
        .iter()
        .map(|item| (item.lane.as_str(), item.gate))
        .collect();
    assert_eq!(
        classes,
        [
            ("macos-gate/merge_group", Some(GateClass::Required)),
            ("Linux (x64)", Some(GateClass::Advisory)),
        ],
        "required first, then advisory"
    );
    let unknown = store.watch("p", 7, Basis::Proxy).expect("watch");
    assert!(
        unknown
            .iter()
            .all(|item| item.gate == Some(GateClass::Unclassified)),
        "{unknown:?}"
    );
}
