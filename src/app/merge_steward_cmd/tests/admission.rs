use super::*;

fn managed_pr(head: &str) -> ObservedPr {
    ObservedPr {
        node_id: "PR_managed".to_owned(),
        fact: StewardPullRequest {
            number: 42,
            head_sha: head.to_owned(),
            head_branch: "feature".to_owned(),
            draft: false,
            merge_state: "BLOCKED".to_owned(),
            auto_merge_active: false,
            queue_position: None,
            labels: vec![MANAGED_LABEL.to_owned()],
            checks: vec![StewardCheck {
                name: HANDOFF_CONTEXT.to_owned(),
                source: crate::merge_steward::StewardCheckSource::StatusContext,
                app_id: None,
                status: "COMPLETED".to_owned(),
                conclusion: Some("SUCCESS".to_owned()),
                run_id: None,
                observed_at: Some("2026-08-15T17:00:00Z".to_owned()),
            }],
        },
        check_rollup_maybe_truncated: false,
    }
}

fn admission_observation() -> RepoObservation {
    let mut observation = observation_for(managed_pr(&"b".repeat(40)), true);
    observation.runs = vec![StewardRun {
        id: 9001,
        workflow_id: 77,
        run_attempt: 1,
        workflow: "Build and Test".to_owned(),
        head_sha: "a".repeat(40),
        head_branch: "feature".to_owned(),
        status: "queued".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(42),
        created_at: "2026-08-15T16:00:00Z".to_owned(),
        jobs: Vec::new(),
    }];
    observation
}

#[test]
fn admission_target_normalizes_case_and_rejects_ambiguous_labels() {
    let normalized = normalize_admission_target(&AdmissionCleanArgs {
        repo: "Owner/repo".to_owned(),
        base: "main".to_owned(),
        labels: vec![
            "self-hosted".to_owned(),
            "ARM64".to_owned(),
            "macOS".to_owned(),
        ],
        apply: false,
    })
    .expect("valid target");
    assert_eq!(normalized.2, vec!["arm64", "macos", "self-hosted"]);

    for labels in [
        vec!["ARM64".to_owned()],
        vec!["self-hosted".to_owned(), "SELF-HOSTED".to_owned()],
        vec!["self-hosted".to_owned(), String::new()],
    ] {
        assert!(
            normalize_admission_target(&AdmissionCleanArgs {
                repo: "owner/repo".to_owned(),
                base: "main".to_owned(),
                labels,
                apply: false,
            })
            .is_err()
        );
    }
}

#[test]
fn prospective_runner_matches_only_queued_subset_labels() {
    let labels = ["self-hosted", "macos", "arm64", "pulp-build-vm"]
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut job = StewardJob {
        name: "macOS".to_owned(),
        status: "queued".to_owned(),
        conclusion: None,
        labels: vec!["self-hosted".to_owned(), "ARM64".to_owned()],
        runner_name: None,
    };
    assert!(job_can_claim_runner(&job, &labels));
    job.labels.push("different-pool".to_owned());
    assert!(!job_can_claim_runner(&job, &labels));
    job.labels.pop();
    job.status = "in_progress".to_owned();
    assert!(!job_can_claim_runner(&job, &labels));
}

#[test]
fn admission_candidates_filter_superseded_runs_by_exact_runner_labels() {
    let temp = tempfile::tempdir().expect("temp");
    let matching = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs/9001/jobs"*)
    printf '%s' '{"jobs":[{"name":"macOS","status":"queued","conclusion":null,"labels":["self-hosted","macOS","ARM64","pulp-build-vm","shipyard-recovery-pool"],"runner_name":null}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let labels = [
        "arm64",
        "macos",
        "pulp-build-vm",
        "self-hosted",
        "shipyard-recovery-pool",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let candidates = admission_candidates(&matching, &admission_observation(), &labels)
        .expect("matching candidate");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].run_id, 9001);

    let nonmatching = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs/9001/jobs"*)
    printf '%s' '{"jobs":[{"name":"other","status":"queued","conclusion":null,"labels":["self-hosted","Linux"],"runner_name":null}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    assert!(
        admission_candidates(&nonmatching, &admission_observation(), &labels)
            .expect("nonmatching candidate")
            .is_empty()
    );
}

#[test]
fn typed_admission_output_matches_tartci_flat_contract() {
    let mut output = Vec::new();
    let exit = emit_admission_verdict(
        &mut output,
        true,
        AdmissionVerdict::Defer,
        "stale_compatible_runs",
        "Generous-Corp/pulp",
        "main",
        &["arm64".to_owned(), "self-hosted".to_owned()],
        &[9001],
    )
    .expect("render verdict");
    assert_eq!(exit, ExitCode::from(3));
    let value: Value = serde_json::from_slice(&output).expect("flat JSON");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["command"], "runner:admission-clean");
    assert_eq!(value["verdict"], "defer");
    assert_eq!(value["reason"], "stale_compatible_runs");
    assert_eq!(value["blocker_run_ids"], json!([9001]));
    assert!(value.get("data").is_none(), "must not use generic envelope");
}
