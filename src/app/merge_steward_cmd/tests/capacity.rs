use super::*;

#[test]
fn required_workflow_capacity_reason_is_rejected_before_github_reads_or_writes() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, r#"echo "unexpected GitHub call: $*" >&2; exit 90"#);
    let observed = queued_run(100, "2026-07-26T00:00:00Z");
    let observation = observation_for(ready_pr(), true);
    let cancellation = RunCancellation {
        run_id: observed.id,
        reason: RunCancellationReason::LowerPriorityBranchPreamble,
    };

    assert!(matches!(
        cancel_capacity_preemption_after_revalidation(
            &actions,
            &observation,
            &cancellation,
            &observed,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "steward:skip",
        ),
        Ok(None)
    ));
}

#[test]
fn second_revalidation_uses_latest_safe_runner_assignment() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let candidate_job_fetches = temp.path().join("candidate-job-fetches");
    let cancelled = temp.path().join("cancelled");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{calls}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[{{"position":1,"enqueuedAt":"2020-01-01T00:00:00Z","headCommit":{{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},"pullRequest":{{"number":42}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[{{"id":200,"workflow_id":88,"run_attempt":1,"name":"Build and Test","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","head_branch":"gh-readonly-queue/main/pr-42","status":"queued","event":"merge_group","pull_requests":[],"created_at":"2026-07-26T00:00:00Z"}}]}}' ;;
  *"actions/runs?status="*)
    printf '%s' '{{"workflow_runs":[]}}' ;;
  *"actions/runs/200/jobs"*)
    printf '%s' '{{"jobs":[{{"name":"macOS build","status":"queued","conclusion":null,"labels":["self-hosted","pulp-build-macos"],"runner_name":""}}]}}' ;;
  "api repos/owner/repo/actions/runs/100")
    if [ -f '{cancelled}' ]; then
      printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":1,"name":"Example validation","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"completed","event":"pull_request","pull_requests":[{{"number":42}}],"created_at":"2026-07-26T00:00:00Z"}}'
    else
      printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":1,"name":"Example validation","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"in_progress","event":"pull_request","pull_requests":[{{"number":42}}],"created_at":"2026-07-26T00:00:00Z"}}'
    fi ;;
  *"actions/runs/100/jobs"*)
    if [ -f '{cancelled}' ]; then
      printf '%s' '{{"jobs":[]}}'
    else
      count=0
      [ ! -f '{candidate_job_fetches}' ] || count=$(sed -n '1p' '{candidate_job_fetches}')
      count=$((count + 1))
      printf '%s\n' "$count" > '{candidate_job_fetches}'
      if [ "$count" -eq 1 ]; then
        printf '%s' '{{"jobs":[{{"name":"preamble","status":"in_progress","conclusion":null,"labels":["self-hosted","pulp-preamble"],"runner_name":"m1"}}]}}'
      else
        printf '%s' '{{"jobs":[{{"name":"preamble","status":"in_progress","conclusion":null,"labels":["self-hosted","pulp-preamble"],"runner_name":"m3"}},{{"name":"hosted setup","status":"completed","conclusion":"success","labels":["ubuntu-latest"],"runner_name":"GitHub Actions 1"}}]}}'
      fi
    fi ;;
  "pr view 42 "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}' ;;
  "pr list "*)
    printf '%s' '[{{"id":"PR_kw","number":42,"isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}]' ;;
  "api -X POST repos/owner/repo/actions/runs/100/cancel")
    : > '{cancelled}'
    printf '%s' '{{}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls = calls.display(),
            candidate_job_fetches = candidate_job_fetches.display(),
            cancelled = cancelled.display(),
        ),
    );
    let mut pr = ready_pr();
    pr.fact.queue_position = Some(1);
    let mut observation = observation_for(pr, true);
    observation.merge_group_heads.insert(42, "b".repeat(40));
    observation.merge_group_enqueued_at.insert(
        42,
        (Utc::now() - chrono::Duration::minutes(20)).to_rfc3339(),
    );
    let mut observed = queued_run(100, "2026-07-26T00:00:00Z");
    observed.status = "in_progress".to_owned();
    observation.runs = vec![observed];
    let cancellation = RunCancellation {
        run_id: 100,
        reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
    };
    let ledger_path = temp.path().join("ledger.json");
    let control = mutation_control(&temp, "studio", "studio");
    let context = CapacityApplyContext {
        actions: &actions,
        observation: &observation,
        cancellation: &cancellation,
        ledger_path: &ledger_path,
        mutation_control: &control,
    };
    let mut ledger = StewardLedger::default();

    let (mutation, error) = apply_capacity_preemption(&context, "steward:skip", &mut ledger);

    assert_eq!(mutation.as_deref(), Some("cancelled_terminal"));
    assert!(error.is_none(), "{error:?}");
    assert!(
        fs::read_to_string(&calls)
            .expect("calls")
            .contains("actions/runs/100/cancel")
    );
    let evidence = ledger
        .audit
        .iter()
        .filter(|entry| {
            entry
                .action
                .starts_with("capacity_preemption_precancel_evidence:")
        })
        .collect::<Vec<_>>();
    assert_eq!(evidence.len(), 2);
    assert!(evidence[0].action.contains("\"runner_name\":\"m1\""));
    assert!(evidence[1].action.contains("\"runner_name\":\"m3\""));
    assert!(evidence[1].action.contains("\"name\":\"hosted setup\""));
}

#[test]
fn final_mutation_boundary_rejects_expensive_job_started_after_evidence_persistence() {
    let temp = tempfile::tempdir().expect("temp");
    let candidate_job_fetches = temp.path().join("candidate-job-fetches");
    let cancelled = temp.path().join("cancelled");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[{{"position":1,"enqueuedAt":"2020-01-01T00:00:00Z","headCommit":{{"oid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}},"pullRequest":{{"number":42}}}}],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[{{"id":200,"workflow_id":88,"run_attempt":1,"name":"Build and Test","head_sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","head_branch":"gh-readonly-queue/main/pr-42","status":"queued","event":"merge_group","pull_requests":[],"created_at":"2026-07-26T00:00:00Z"}}]}}' ;;
  *"actions/runs?status="*)
    printf '%s' '{{"workflow_runs":[]}}' ;;
  *"actions/runs/200/jobs"*)
    printf '%s' '{{"jobs":[{{"name":"macOS build","status":"queued","conclusion":null,"labels":["self-hosted","pulp-build-macos"],"runner_name":""}}]}}' ;;
  "api repos/owner/repo/actions/runs/100")
    printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":1,"name":"Example validation","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"in_progress","event":"pull_request","pull_requests":[{{"number":42}}],"created_at":"2026-07-26T00:00:00Z"}}' ;;
  *"actions/runs/100/jobs"*)
    count=0
    [ ! -f '{candidate_job_fetches}' ] || count=$(sed -n '1p' '{candidate_job_fetches}')
    count=$((count + 1))
    printf '%s\n' "$count" > '{candidate_job_fetches}'
    if [ "$count" -le 2 ]; then
      printf '%s' '{{"jobs":[{{"name":"preamble","status":"in_progress","conclusion":null,"labels":["self-hosted","pulp-preamble"],"runner_name":"m1"}}]}}'
    else
      printf '%s' '{{"jobs":[{{"name":"preamble","status":"completed","conclusion":"success","labels":["self-hosted","pulp-preamble"],"runner_name":"m1"}},{{"name":"expensive build","status":"in_progress","conclusion":null,"labels":["self-hosted","pulp-build-macos"],"runner_name":"m3"}}]}}'
    fi ;;
  "pr view 42 "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}' ;;
  "pr list "*)
    printf '%s' '[{{"id":"PR_kw","number":42,"isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}]' ;;
  "api -X POST repos/owner/repo/actions/runs/100/cancel")
    : > '{cancelled}'
    echo "unsafe cancellation reached POST" >&2
    exit 9 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            candidate_job_fetches = candidate_job_fetches.display(),
            cancelled = cancelled.display(),
        ),
    );
    let mut pr = ready_pr();
    pr.fact.queue_position = Some(1);
    let mut observation = observation_for(pr, true);
    observation.merge_group_heads.insert(42, "b".repeat(40));
    observation.merge_group_enqueued_at.insert(
        42,
        (Utc::now() - chrono::Duration::minutes(20)).to_rfc3339(),
    );
    let mut observed = queued_run(100, "2026-07-26T00:00:00Z");
    observed.status = "in_progress".to_owned();
    observation.runs = vec![observed];
    let cancellation = RunCancellation {
        run_id: 100,
        reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
    };
    let ledger_path = temp.path().join("ledger.json");
    let control = mutation_control(&temp, "studio", "studio");
    let context = CapacityApplyContext {
        actions: &actions,
        observation: &observation,
        cancellation: &cancellation,
        ledger_path: &ledger_path,
        mutation_control: &control,
    };
    let mut ledger = StewardLedger::default();

    let (mutation, error) = apply_capacity_preemption(&context, "steward:skip", &mut ledger);

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_attempt_revalidation")
    );
    assert!(error.is_none(), "{error:?}");
    assert!(!cancelled.exists(), "unsafe cancellation reached POST");
    assert_eq!(
        fs::read_to_string(&candidate_job_fetches).expect("fetch count"),
        "3\n"
    );
    assert!(ledger.pending_cancellations.is_empty());
}
