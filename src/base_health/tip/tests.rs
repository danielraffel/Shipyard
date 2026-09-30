use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::*;

const REPO: &str = "o/r";
const TIP: &str = "6e95d5cab0000000000000000000000000000000";

/// A reader over canned responses keyed by API path (query string dropped).
fn reader(responses: BTreeMap<String, String>) -> impl Fn(&[String]) -> Result<String, String> {
    move |args: &[String]| {
        let path = args.get(1).ok_or("no path")?;
        let key = path.split('?').next().unwrap_or(path);
        responses
            .get(key)
            .cloned()
            .ok_or_else(|| format!("gh: Not Found (HTTP 404) {key}"))
    }
}

fn run(id: u64, name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({
        "id": id, "name": name, "event": "merge_group", "head_sha": TIP,
        "status": status, "conclusion": conclusion,
        "html_url": format!("https://example.test/runs/{id}"),
    })
}

fn job(id: u64, name: &str, status: &str, conclusion: Option<&str>) -> Value {
    json!({"id": id, "name": name, "status": status, "conclusion": conclusion,
           "html_url": format!("https://example.test/jobs/{id}")})
}

fn repo(runs: &[Value], jobs: &[(u64, Vec<Value>)]) -> BTreeMap<String, String> {
    let mut responses = BTreeMap::from([
        (
            format!("repos/{REPO}/commits/main"),
            json!({"sha": TIP}).to_string(),
        ),
        (
            format!("repos/{REPO}/actions/runs"),
            json!({"workflow_runs": runs}).to_string(),
        ),
    ]);
    for (run_id, list) in jobs {
        responses.insert(
            format!("repos/{REPO}/actions/runs/{run_id}/jobs"),
            json!({"jobs": list}).to_string(),
        );
    }
    responses
}

fn required() -> Vec<String> {
    vec!["macos".to_owned(), "Vellum freeze".to_owned()]
}

/// The Pulp shape: the build run concludes `failure` because the advisory
/// Linux leg failed, while the required `macos` job passed.
fn green_gate_red_advisory() -> BTreeMap<String, String> {
    repo(
        &[
            run(1, "Build and Test", "completed", Some("failure")),
            run(2, "Vellum freeze", "completed", Some("success")),
        ],
        &[
            (
                1,
                vec![
                    job(10, "classify", "completed", Some("success")),
                    job(11, "macos", "completed", Some("success")),
                    job(
                        12,
                        "Linux (x64) [github-hosted]",
                        "completed",
                        Some("failure"),
                    ),
                    job(13, "windows", "completed", Some("skipped")),
                ],
            ),
            (
                2,
                vec![job(20, "Vellum freeze", "completed", Some("success"))],
            ),
        ],
    )
}

#[test]
fn green_required_gate_with_red_advisory_is_healthy() {
    let gh = reader(green_gate_red_advisory());
    let health = read_tip(&gh, REPO, "main", &required());
    assert_eq!(health.verdict, TipVerdict::Healthy);
    assert_eq!(health.tip_sha.as_deref(), Some(TIP));
    assert_eq!(health.runs[0].conclusion.as_deref(), Some("failure"));
    assert_eq!(health.jobs.len(), 2);
    // commit + runs + two job listings; no log read on a healthy tip.
    assert_eq!(health.api_calls, 4);
}

#[test]
fn failed_required_gate_is_red_with_test_names() {
    let mut responses = repo(
        &[run(1, "Build and Test", "completed", Some("failure"))],
        &[(
            1,
            vec![
                job(11, "macos", "completed", Some("failure")),
                job(20, "Vellum freeze", "completed", Some("success")),
            ],
        )],
    );
    responses.insert(
        format!("repos/{REPO}/actions/jobs/11/logs"),
        "2026-09-29T05:00:00.0000000Z The following tests FAILED:\n\
         2026-09-29T05:00:00.0000000Z \t 42 - pulp-test-widgets (Failed)\n\
         2026-09-29T05:00:00.0000000Z Errors while running CTest\n"
            .to_owned(),
    );
    let gh = reader(responses);
    let health = read_tip(&gh, REPO, "main", &required());
    let TipVerdict::Red { failing } = &health.verdict else {
        panic!("expected red, got {:?}", health.verdict);
    };
    assert_eq!(failing.len(), 1);
    assert_eq!(failing[0].context, "macos");
    assert_eq!(failing[0].conclusion, "failure");
    assert_eq!(failing[0].tests, vec!["42 - pulp-test-widgets (Failed)"]);
}

#[test]
fn red_survives_an_unreadable_log() {
    let gh = reader(repo(
        &[run(1, "Build and Test", "completed", Some("failure"))],
        &[(
            1,
            vec![
                job(11, "macos", "completed", Some("cancelled")),
                job(20, "Vellum freeze", "completed", Some("success")),
            ],
        )],
    ));
    let health = read_tip(&gh, REPO, "main", &required());
    let TipVerdict::Red { failing } = &health.verdict else {
        panic!("expected red, got {:?}", health.verdict);
    };
    assert!(failing[0].tests.is_empty());
}

#[test]
fn tip_without_a_merge_group_run_is_unproven() {
    let gh = reader(repo(&[], &[]));
    let health = read_tip(&gh, REPO, "main", &required());
    let TipVerdict::Unproven { detail } = &health.verdict else {
        panic!("expected unproven, got {:?}", health.verdict);
    };
    assert!(detail.contains(TIP), "{detail}");
    assert!(detail.contains("direct or admin push"), "{detail}");
}

#[test]
fn runs_for_another_commit_do_not_speak_for_the_tip() {
    let mut other = run(1, "Build and Test", "completed", Some("success"));
    other["head_sha"] = Value::from("ffffffffffffffffffffffffffffffffffffffff");
    let gh = reader(repo(&[other], &[]));
    let health = read_tip(&gh, REPO, "main", &required());
    assert!(matches!(health.verdict, TipVerdict::Unproven { .. }));
}

#[test]
fn running_required_gate_is_pending() {
    let gh = reader(repo(
        &[
            run(1, "Build and Test", "in_progress", None),
            run(2, "Vellum freeze", "completed", Some("success")),
        ],
        &[
            (1, vec![job(11, "macos", "in_progress", None)]),
            (
                2,
                vec![job(20, "Vellum freeze", "completed", Some("success"))],
            ),
        ],
    ));
    let health = read_tip(&gh, REPO, "main", &required());
    assert_eq!(
        health.verdict,
        TipVerdict::Pending {
            waiting: vec!["macos".to_owned()]
        }
    );
}

#[test]
fn required_job_not_yet_created_on_a_running_run_is_pending() {
    let gh = reader(repo(
        &[run(1, "Build and Test", "in_progress", None)],
        &[(
            1,
            vec![
                job(10, "classify", "completed", Some("success")),
                job(20, "Vellum freeze", "completed", Some("success")),
            ],
        )],
    ));
    let health = read_tip(&gh, REPO, "main", &required());
    assert!(matches!(health.verdict, TipVerdict::Pending { .. }));
}

#[test]
fn required_job_that_never_ran_is_unproven_not_healthy() {
    let gh = reader(repo(
        &[run(1, "Build and Test", "completed", Some("success"))],
        &[(
            1,
            vec![
                job(11, "macos", "completed", Some("skipped")),
                job(20, "Vellum freeze", "completed", Some("success")),
            ],
        )],
    ));
    let health = read_tip(&gh, REPO, "main", &required());
    let TipVerdict::Unproven { detail } = &health.verdict else {
        panic!("expected unproven, got {:?}", health.verdict);
    };
    assert!(detail.contains("`macos` was skipped"), "{detail}");
}

#[test]
fn unreadable_commit_is_unknown_not_healthy() {
    let gh = reader(BTreeMap::new());
    let health = read_tip(&gh, REPO, "main", &required());
    assert!(matches!(health.verdict, TipVerdict::Unreadable { .. }));
    assert_eq!(health.verdict.label(), "UNKNOWN");
}

#[test]
fn no_required_contexts_is_unproven() {
    let gh = reader(green_gate_red_advisory());
    let health = read_tip(&gh, REPO, "main", &[]);
    assert!(matches!(health.verdict, TipVerdict::Unproven { .. }));
}
