use super::*;
#[cfg(unix)]
use crate::app::merge_steward_cmd::cancellation::{apply_pr_plans, apply_run_cancellation};
use crate::app::merge_steward_cmd::cancellation_recovery::resume_pending_cancellation;
#[cfg(unix)]
use crate::app::merge_steward_cmd::cancellation_recovery::{
    pending_uncertainty, resolve_rejected_pending_intent, resume_force_cancel_after_normal_wait,
};
use crate::app::merge_steward_cmd::cancellation_revalidation::same_workflow_attempt;
#[cfg(unix)]
use crate::app::merge_steward_cmd::cancellation_terminalization::force_cancel_nonterminal_run;
use crate::app::merge_steward_cmd::capacity_cancellation::{
    pending_cancellation_key, start_capacity_preemption,
};
use crate::app::merge_steward_cmd::handoff::TerminalOwnerRoute;
#[cfg(unix)]
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
#[cfg(unix)]
use crate::app::merge_steward_cmd::pr_mutations::mutate_pr_with_recovery;
use crate::app::merge_steward_cmd::pr_mutations::rollback_transient_attempt;
use crate::app::merge_steward_cmd::pr_mutations::run_attempt_allows_transient_rerun;
use crate::app::merge_steward_cmd::terminal_handoff::{
    persist_actionable_failure, persist_success_continuation, resolve_terminal_handoffs,
};
use crate::merge_steward::StewardCheckSource;

fn terminal_owner_route(route_id: &str) -> TerminalOwnerRoute {
    TerminalOwnerRoute {
        origin_machine: "m3".to_owned(),
        owner_id: "owner-exact".to_owned(),
        ownership_generation: 1,
        owner_disposition: "original_owner".to_owned(),
        route_id: Some(route_id.to_owned()),
        provider: Some("codex".to_owned()),
        resume_transport: Some("codex_queue".to_owned()),
        terminal_provenance: Some(TerminalProvenanceKind::Absent),
        provider_route: None,
    }
}

#[cfg(unix)]
fn recovery_witness() -> QueueWitness {
    let now = Utc::now();
    QueueWitness {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        base_sha: "dddddddddddddddddddddddddddddddddddddddd".to_owned(),
        pr_number: 42,
        pr_head: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        merge_group_head: "cccccccccccccccccccccccccccccccccccccccc".to_owned(),
        position: 1,
        enqueued_at: (now - chrono::Duration::minutes(20)).to_rfc3339(),
        observed_at: (now - chrono::Duration::minutes(15)).to_rfc3339(),
        required_checks: vec![WitnessRequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
    }
}

#[cfg(unix)]
#[test]
fn recovery_witnesses_only_the_queue_front() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"repos/owner/repo/commits/main"*) printf '%s' '{"sha":"dddddddddddddddddddddddddddddddddddddddd"}' ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '[[{"type":"required_status_checks","parameters":{"required_status_checks":[{"context":"macos"}]}}]]' ;;
  *"branches/main/protection/required_status_checks"*) printf '%s' '{"contexts":["macos"],"checks":[]}' ;;
  *"mergeQueue"*) printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[{"position":2,"enqueuedAt":"2026-08-25T21:40:00Z","headCommit":{"oid":"cccccccccccccccccccccccccccccccccccccccc"},"pullRequest":{"number":42}}],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  *) printf '%s' '{}' ;;
esac
"#,
    );
    let mut pr = ready_pr();
    pr.fact.queue_position = Some(2);
    pr.fact.labels.push(MANAGED_LABEL.to_owned());
    pr.fact.checks.push(StewardCheck {
        name: HANDOFF_CONTEXT.to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        check_run_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: None,
        observed_at: Some("2026-08-25T21:39:00Z".to_owned()),
    });
    let mut observation = observation_for(pr, true);
    observation
        .merge_group_heads
        .insert(42, "cccccccccccccccccccccccccccccccccccccccc".to_owned());
    observation
        .merge_group_enqueued_at
        .insert(42, "2026-08-25T21:40:00Z".to_owned());
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: true,
        coalesce: true,
        preempt_capacity: true,
        max_preemptions_per_head: 1,
        apply: true,
        ledger: None,
    };
    let mut ledger = StewardLedger::default();

    crate::app::merge_steward_cmd::queue_priority_recovery::record_queue_witnesses(
        &actions,
        &observation,
        &args,
        &temp.path().join("ledger.json"),
        &mut ledger,
    )
    .expect("witness scan");

    assert!(ledger.queue_witnesses.is_empty());
}

#[cfg(unix)]
fn queue_recovery_gh(
    temp: &tempfile::TempDir,
    setup_log: &str,
    labels: &str,
) -> (GitHubActions, PathBuf, QueueWitness) {
    let log = temp.path().join("calls");
    let base_reads = temp.path().join("base-reads");
    let base_drift = temp.path().join("base-drift");
    let admission_mismatch = temp.path().join("admission-mismatch");
    let timeline_error = temp.path().join("timeline-error");
    let final_requeued = temp.path().join("final-requeued");
    let final_requeued_after_intent = temp.path().join("final-requeued-after-intent");
    let final_authority_reads = temp.path().join("final-authority-reads");
    let witness = recovery_witness();
    let mismatch_admission = (DateTime::parse_from_rfc3339(&witness.enqueued_at)
        .expect("enqueue time")
        + chrono::Duration::minutes(1))
    .to_rfc3339();
    let run_created = (Utc::now() - chrono::Duration::minutes(18)).to_rfc3339();
    let run_updated = (Utc::now() - chrono::Duration::minutes(2)).to_rfc3339();
    let removed_at = (Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
    let actions = fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"repos/owner/repo/commits/main"*)
    reads=0; test ! -f '{}' || reads=$(cat '{}'); reads=$((reads + 1)); printf '%s' "$reads" > '{}'
    if test -f '{}' && test "$reads" -gt 1; then sha=eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee; else sha=dddddddddddddddddddddddddddddddddddddddd; fi
    printf '{{"sha":"%s"}}' "$sha" ;;
  *"rules/branches/main --paginate --slurp"*) printf '%s' '[[{{"type":"required_status_checks","parameters":{{"required_status_checks":[{{"context":"macos"}}]}}}}]]' ;;
  *"branches/main/protection/required_status_checks"*) printf '%s' '{{"contexts":["macos"],"checks":[]}}' ;;
  *"timelineItems"*)
    if test -f '{}'; then printf '%s' '{{"malformed":true}}'; exit 0; fi
    if test -f '{}'; then admitted='{}'; else admitted='{}'; fi
    printf '{{"data":{{"repository":{{"pullRequest":{{"timelineItems":{{"nodes":[{{"__typename":"AddedToMergeQueueEvent","createdAt":"%s"}},{{"__typename":"RemovedFromMergeQueueEvent","reason":"failed_checks","createdAt":"{}"}}],"pageInfo":{{"hasPreviousPage":false}}}}}}}}}}}}' "$admitted" ;;
  *"actions/runs?event=merge_group"*) printf '%s' '{{"workflow_runs":[{{"id":32903260905,"event":"merge_group","status":"completed","conclusion":"failure","head_sha":"cccccccccccccccccccccccccccccccccccccccc","created_at":"{}","updated_at":"{}"}}]}}' ;;
  *"commits/cccccccccccccccccccccccccccccccccccccccc/check-runs"*) printf '%s' '{{"total_count":1,"check_runs":[{{"id":901,"name":"macos","status":"completed","conclusion":"failure","details_url":"https://github.com/owner/repo/actions/runs/32903260905","completed_at":"{}","app":{{"id":15368}}}}]}}' ;;
  *"commits/cccccccccccccccccccccccccccccccccccccccc/statuses"*) printf '%s' '[]' ;;
  *"actions/runs/32903260905/jobs"*) printf '%s' '{{"jobs":[{{"id":97981596587,"conclusion":"failure","runner_group_name":"GitHub Actions","labels":{},"steps":[{{"name":"Set up job","status":"completed","conclusion":"failure"}}]}}]}}' ;;
  *"actions/jobs/97981596587/logs"*) printf '%s' '{}' ;;
  *"finalRecoveryAuthority"*)
    reads=0; test ! -f '{}' || reads=$(cat '{}'); reads=$((reads + 1)); printf '%s' "$reads" > '{}'
    if test -f '{}' || (test -f '{}' && test "$reads" -gt 1); then nodes='[{{"pullRequest":{{"number":42}}}}]'; else nodes='[]'; fi
    printf '{{"data":{{"repository":{{"ref":{{"target":{{"oid":"dddddddddddddddddddddddddddddddddddddddd"}}}},"pullRequest":{{"state":"OPEN","baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},"mergeQueue":{{"entries":{{"nodes":%s,"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' "$nodes" ;;
  *"query=query("*"mergeQueue"*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*) printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":1}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display(),
            base_reads.display(),
            base_reads.display(),
            base_reads.display(),
            base_drift.display(),
            timeline_error.display(),
            admission_mismatch.display(),
            mismatch_admission,
            witness.enqueued_at,
            removed_at,
            run_created,
            run_updated,
            run_updated,
            labels,
            setup_log,
            final_authority_reads.display(),
            final_authority_reads.display(),
            final_authority_reads.display(),
            final_requeued.display(),
            final_requeued_after_intent.display()
        ),
    );
    (actions, log, witness)
}

#[cfg(unix)]
#[test]
fn recovery_refuses_jump_when_pr_reappears_in_final_queue_snapshot() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(temp.path().join("final-requeued"), "queued").expect("marker");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        true,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_final_mutable_authority")
    );
    assert!(error.is_none(), "{error:?}");
    assert!(ledger.queue_recovery_receipts.is_empty());
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(!calls.contains("jump:true"), "{calls}");
}

#[cfg(unix)]
#[test]
fn recovery_revalidates_queue_absence_after_persisting_the_intent() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(temp.path().join("final-requeued-after-intent"), "queued").expect("marker");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        true,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_recovery_receipt_mutable_authority")
    );
    assert!(error.is_none(), "{error:?}");
    assert!(
        ledger
            .queue_recovery_receipts
            .values()
            .all(|receipt| receipt.phase == QueueRecoveryPhase::Intent)
    );
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(!calls.contains("jump:true"), "{calls}");
}

#[cfg(unix)]
#[test]
fn unreadable_optional_recovery_proof_preserves_ordinary_exact_head_enqueue() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(temp.path().join("timeline-error"), "error").expect("marker");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        true,
    );

    assert_eq!(mutation.as_deref(), Some("enqueued"));
    assert!(error.is_none(), "{error:?}");
    assert!(ledger.queue_recovery_receipts.is_empty());
    assert!(
        ledger
            .audit
            .iter()
            .any(|entry| { entry.action == "queue_priority_recovery_unreadable_fell_back" })
    );
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(calls.contains("enqueuePullRequest"), "{calls}");
    assert!(!calls.contains("jump:true"), "{calls}");
}

#[cfg(unix)]
#[test]
fn recovery_rejects_base_drift_at_the_final_guarded_evidence_read() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(temp.path().join("base-drift"), "drift").expect("drift marker");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        true,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_final_recovery_revalidation")
    );
    assert!(error.is_none(), "{error:?}");
    assert!(ledger.queue_recovery_receipts.is_empty());
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn recovery_rejects_a_failed_checks_removal_from_another_admission() {
    let temp = tempfile::tempdir().expect("temp");
    fs::write(temp.path().join("admission-mismatch"), "mismatch").expect("marker");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);

    let evidence = crate::app::merge_steward_cmd::queue_priority_recovery::recovery_evidence(
        &actions,
        &observation,
        &pr,
        &ledger,
    )
    .expect("proof read succeeds");

    assert!(evidence.is_none());
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(!calls.contains("actions/runs?event=merge_group"), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn hosted_precheckout_eviction_restores_priority_once_with_write_ahead_receipt() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        true,
    );

    assert_eq!(mutation.as_deref(), Some("enqueued_priority_restored"));
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(calls.contains("jump:true"), "{calls}");
    assert_eq!(calls.matches("enqueuePullRequest").count(), 1, "{calls}");
    let saved: StewardLedger =
        serde_json::from_slice(&fs::read(ledger_path).expect("ledger")).expect("valid ledger");
    assert_eq!(saved.queue_recovery_receipts.len(), 1);
    assert!(saved.queue_recovery_receipts.values().all(|receipt| {
        receipt.phase == QueueRecoveryPhase::Accepted
            && receipt.run_id == 32_903_260_905
            && receipt.job_id == 97_981_596_587
    }));
}

#[cfg(unix)]
#[test]
fn hosted_precheckout_priority_recovery_obeys_central_hold_before_receipt_or_jump() {
    let temp = tempfile::tempdir().expect("temp");
    let (actions, calls_path, witness) = queue_recovery_gh(
        &temp,
        "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
        "[\"ubuntu-latest\"]",
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    ledger
        .queue_witnesses
        .insert("owner/repo#42".to_owned(), witness);
    let control = mutation_control(&temp, "studio", "studio");
    let state_root = control.store.path().parent().expect("state root");
    crate::merge_queue_control::hold(state_root, "incident").expect("hold");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    let (mutation, error) = mutate_pr_with_recovery(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
        true,
    );

    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("centrally held")),
        "{error:?}"
    );
    assert!(ledger.queue_recovery_receipts.is_empty());
    let calls = fs::read_to_string(calls_path).expect("calls");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn generic_setup_failure_and_self_hosted_job_never_request_jump() {
    for (setup_log, labels) in [
        ("The action version does not exist", "[\"ubuntu-latest\"]"),
        (
            "Name or service not known (internal-api.service.iad.github.net:443); Failed to download archive after 3 attempts",
            "[\"self-hosted\",\"linux\"]",
        ),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let (actions, calls_path, witness) = queue_recovery_gh(&temp, setup_log, labels);
        let pr = ready_pr();
        let observation = observation_for(pr.clone(), true);
        let mut ledger = StewardLedger::default();
        ledger
            .queue_witnesses
            .insert("owner/repo#42".to_owned(), witness);
        let evidence = crate::app::merge_steward_cmd::queue_priority_recovery::recovery_evidence(
            &actions,
            &observation,
            &pr,
            &ledger,
        )
        .expect("proof read succeeds");
        assert!(evidence.is_none());
        let calls = fs::read_to_string(calls_path).expect("calls");
        assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    }
}

#[test]
fn github_run_attempt_fences_lost_transient_retry_ledger() {
    assert!(!run_attempt_allows_transient_rerun(1, 0));
    assert!(run_attempt_allows_transient_rerun(1, 1));
    assert!(!run_attempt_allows_transient_rerun(2, 1));
    assert!(run_attempt_allows_transient_rerun(2, 2));
    assert!(!run_attempt_allows_transient_rerun(3, 2));
}

#[test]
fn managed_ownership_uses_the_configured_label_and_handoff_context() {
    let mut pr = ready_pr();
    pr.fact.labels.push("custom:managed".to_owned());
    pr.fact.checks.push(StewardCheck {
        name: "custom/handoff".to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        check_run_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: None,
        observed_at: Some("2026-07-26T00:00:00Z".to_owned()),
    });

    assert!(pull_request_is_managed(
        &pr,
        "custom:managed",
        "custom/handoff"
    ));
    assert!(!pull_request_is_managed(
        &pr,
        MANAGED_LABEL,
        HANDOFF_CONTEXT
    ));
}

#[test]
fn overlapping_apply_pass_fails_fast_on_ledger_lock() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger = temp.path().join("merge-steward.json");
    let _first = acquire_ledger_lock(&ledger).expect("first lock");
    let error = acquire_ledger_lock(&ledger).expect_err("second lock must not block");
    assert!(
        error.message.contains("already running"),
        "{}",
        error.message
    );
}

#[test]
fn ledger_save_replaces_an_existing_file_portably() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger_path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    save_ledger(&ledger_path, &ledger).expect("initial ledger");
    ledger
        .transient_attempts
        .insert("Generous-Corp/pulp#1:head:2".to_owned(), 1);
    save_ledger(&ledger_path, &ledger).expect("replacement ledger");
    assert_eq!(
        load_ledger(&ledger_path)
            .expect("load replacement")
            .transient_attempts,
        ledger.transient_attempts
    );
}

#[test]
fn final_ledger_failure_is_renderable_and_marks_tick_unhealthy() {
    let temp = tempfile::tempdir().expect("temp");
    let parent_file = temp.path().join("not-a-directory");
    fs::write(&parent_file, "occupied").expect("parent file");
    let ledger_path = parent_file.join("merge-steward.json");
    let mut reports = Vec::new();
    let mut unhealthy = false;

    persist_final_ledger(
        &ledger_path,
        &StewardLedger::default(),
        "main",
        &mut reports,
        &mut unhealthy,
    );

    assert!(unhealthy);
    assert_eq!(reports.len(), 1);
    assert!(reports[0].errors[0].contains("ledger persistence failed"));
    let mut output = Vec::new();
    render_report(&mut output, true, true, &ledger_path, &reports).expect("render");
    assert!(
        String::from_utf8(output)
            .expect("UTF-8")
            .contains("ledger persistence failed")
    );
}

#[test]
fn failed_terminal_handoff_write_restores_prior_in_memory_obligation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    persist_actionable_failure(
        &path,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        head,
        Some(terminal_owner_route("route-a")),
        vec!["windows@app=9".to_owned()],
    )
    .expect("first");
    let blocked_parent = temp.path().join("not-a-directory");
    fs::write(&blocked_parent, "occupied").expect("blocked parent");
    let blocked_ledger = blocked_parent.join("ledger.json");
    resolve_terminal_handoffs(&blocked_ledger, &mut ledger, "owner/repo", "main", 7, head)
        .expect_err("resolution save fails");
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("still recorded")
            .phase,
        TerminalHandoffPhase::Recorded
    );
    persist_actionable_failure(
        &blocked_ledger,
        &mut ledger,
        "owner/repo",
        "main",
        7,
        head,
        Some(terminal_owner_route("route-a")),
        vec!["macos@app=42".to_owned()],
    )
    .expect_err("replacement save fails");
    assert_eq!(ledger.terminal_handoffs.len(), 1);
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("prior failure")
            .failure_contexts,
        vec!["windows@app=9"]
    );
}

#[test]
fn restart_preserves_success_and_failure_terminal_handoffs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("merge-steward.json");
    let mut ledger = StewardLedger::default();
    let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    persist_success_continuation(
        &path,
        &mut ledger,
        "Owner/Repo",
        "main",
        7,
        head,
        Some(terminal_owner_route("route-success")),
    )
    .expect("success continuation");
    persist_actionable_failure(
        &path,
        &mut ledger,
        "Owner/Repo",
        "main",
        8,
        head,
        Some(terminal_owner_route("route-failure")),
        vec!["macos@app=42".to_owned()],
    )
    .expect("failure wake");

    let restarted = crate::app::merge_steward_cmd::ledger::load_ledger(&path).expect("restart");
    assert_eq!(restarted.terminal_handoffs.len(), 2);
    assert!(restarted.terminal_handoffs.values().any(|record| {
        record.outcome == TerminalHandoffOutcome::SuccessContinuation
            && record.phase == TerminalHandoffPhase::Pending
            && !record.wake_consumer_available
    }));
    assert!(restarted.terminal_handoffs.values().any(|record| {
        record.outcome == TerminalHandoffOutcome::ActionableFailure
            && record.phase == TerminalHandoffPhase::Recorded
            && record.owner_route_id.as_deref() == Some("route-failure")
            && record.ownership_generation == Some(1)
            && !record.wake_consumer_available
    }));
}

#[cfg(unix)]
#[test]
fn stale_exclusion_cannot_resolve_a_newly_actionable_terminal_handoff() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"FAILURE","completedAt":"2026-08-27T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/101"}]}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let mut pr = ready_pr();
    pr.fact.labels.push("steward:skip".to_owned());
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &ledger_path,
        &mut ledger,
        "owner/repo",
        "main",
        pr.fact.number,
        &pr.fact.head_sha,
        Some(terminal_owner_route("route-failure")),
        vec!["macos@app=unbound".to_owned()],
    )
    .expect("failure wake");
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = reconcile_recovery_signal(
        &context,
        &pr,
        &policy,
        &StewardDecision::OptedOut,
        &mut ledger,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("recovery_skipped_after_live_revalidation")
    );
    assert!(error.is_none(), "{error:?}");
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("failure remains actionable")
            .phase,
        TerminalHandoffPhase::Recorded
    );
}

#[cfg(unix)]
#[test]
fn github_recovery_clear_resolves_terminal_handoff_inside_the_clear_fence() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{"name":"shipyard:needs-agent"}],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-08-27T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/101"},{"__typename":"StatusContext","context":"shipyard/recovery","state":"FAILURE","createdAt":"2026-08-27T00:00:01Z"}]}' ;;
  *"statuses/"*) printf '%s' '{}' ;;
  "api repos/owner/repo/pulls/42")
    printf '%s' '{"state":"open","head":{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}}' ;;
  *"issues/42/labels/shipyard%3Aneeds-agent"*) printf '%s' '{}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let mut pr = ready_pr();
    pr.fact.labels.push(NEEDS_AGENT_LABEL.to_owned());
    pr.fact.checks.push(StewardCheck {
        name: RECOVERY_CONTEXT.to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        check_run_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: None,
        observed_at: Some("2026-08-27T00:00:01Z".to_owned()),
    });
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    persist_actionable_failure(
        &ledger_path,
        &mut ledger,
        "owner/repo",
        "main",
        pr.fact.number,
        &pr.fact.head_sha,
        Some(terminal_owner_route("route-failure")),
        vec!["macos@app=unbound".to_owned()],
    )
    .expect("failure wake");
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = reconcile_recovery_signal(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert!(error.is_none(), "mutation={mutation:?} error={error:?}");
    assert_eq!(mutation.as_deref(), Some("needs_agent_cleared"));
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("resolved wake")
            .phase,
        TerminalHandoffPhase::Resolved
    );
}

#[cfg(unix)]
#[test]
fn contended_terminal_publication_lease_cannot_report_healthy_without_an_obligation() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, r#"echo "unexpected GitHub call: $*" >&2; exit 90"#);
    let mut pr = ready_pr();
    pr.fact.labels.push(NEEDS_AGENT_LABEL.to_owned());
    pr.fact.checks[0].conclusion = Some("FAILURE".to_owned());
    pr.fact.checks.push(StewardCheck {
        name: RECOVERY_CONTEXT.to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        check_run_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: None,
        observed_at: Some("2026-08-27T00:00:01Z".to_owned()),
    });
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let decision = StewardDecision::RequiredFailed {
        contexts: vec!["macos".to_owned()],
    };
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);
    let publication_lease =
        crate::app::merge_steward_cmd::recovery_worker::acquire_recovery_publication_lease(
            &mutation_control.state_dir,
        )
        .expect("first publication lease");

    let (mutation, error) =
        reconcile_recovery_signal(&context, &pr, &policy, &decision, &mut ledger);

    drop(publication_lease);
    assert!(mutation.is_none(), "{mutation:?}");
    assert!(
        error.as_deref().is_some_and(
            |error| error.contains("mandatory terminal handoff publication lease failed")
        ),
        "{error:?}"
    );
    assert!(
        ledger.terminal_handoffs.is_empty(),
        "the error is mandatory precisely because no durable wake could be recorded"
    );
    assert!(!ledger_path.exists());
}

#[cfg(unix)]
#[test]
fn terminal_revalidation_failure_stays_unhealthy_until_retry_publishes_the_wake() {
    let temp = tempfile::tempdir().expect("temp");
    let failed_once = temp.path().join("failed-once");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
case "$*" in
  *"query=query("*"mergeQueue"*)
    if [ ! -f '{failed_once}' ]; then
      : > '{failed_once}'
      echo "temporary GitHub read failure" >&2
      exit 1
    fi
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:needs-agent"}}],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"FAILURE","completedAt":"2026-08-27T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/101"}},{{"__typename":"StatusContext","context":"shipyard/steward-recovery","state":"FAILURE","createdAt":"2026-08-27T00:00:01Z"}}]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            failed_once = failed_once.display(),
        ),
    );
    let mut pr = ready_pr();
    pr.fact.labels.push(NEEDS_AGENT_LABEL.to_owned());
    pr.fact.checks[0].conclusion = Some("FAILURE".to_owned());
    pr.fact.checks.push(StewardCheck {
        name: RECOVERY_CONTEXT.to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: None,
        check_run_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: None,
        observed_at: Some("2026-08-27T00:00:01Z".to_owned()),
    });
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let decision = StewardDecision::RequiredFailed {
        contexts: vec!["macos".to_owned()],
    };
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let first = reconcile_recovery_signal(&context, &pr, &policy, &decision, &mut ledger);
    assert!(first.0.is_none(), "{first:?}");
    assert!(
        first.1.as_deref().is_some_and(
            |error| error.contains("mandatory terminal handoff live revalidation failed")
        ),
        "{first:?}"
    );
    assert!(ledger.terminal_handoffs.is_empty());
    assert!(!ledger_path.exists());

    let second = reconcile_recovery_signal(&context, &pr, &policy, &decision, &mut ledger);
    assert!(
        second
            .0
            .as_deref()
            .is_some_and(|mutation| mutation.starts_with("recovery_request_deferred:")),
        "optional model recovery may defer only after the wake is durable: {second:?}"
    );
    assert!(second.1.is_none(), "{second:?}");
    let wake = ledger
        .terminal_handoffs
        .values()
        .next()
        .expect("durable retry wake");
    assert_eq!(wake.phase, TerminalHandoffPhase::Recorded);
    assert!(!wake.wake_consumer_available);
    assert!(ledger_path.exists());
}

#[cfg(unix)]
#[test]
fn enqueue_transport_mutates_only_after_live_queue_and_head_revalidation() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":1}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert_eq!(mutation.as_deref(), Some("enqueued"));
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert!(calls.contains("mergeQueue"), "{calls}");
    assert!(calls.contains("pr view 42"), "{calls}");
    assert!(calls.contains("enqueuePullRequest"), "{calls}");
    let handoff = ledger
        .terminal_handoffs
        .values()
        .next()
        .expect("durable success continuation");
    assert_eq!(handoff.repo, "owner/repo");
    assert_eq!(handoff.pr_number, 42);
    assert_eq!(handoff.head_sha, pr.fact.head_sha);
    assert_eq!(handoff.phase, TerminalHandoffPhase::Applied);
    assert_eq!(handoff.owner_disposition, "route_registry_required");
    assert!(!handoff.wake_consumer_available);
}

#[cfg(unix)]
#[test]
fn enqueue_revalidation_refuses_a_new_current_head_provenance_blocker() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"5·UnReSoLvEd"}}],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert_eq!(mutation.as_deref(), Some("skipped_after_live_revalidation"));
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert!(calls.contains("pr view 42"), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn enqueue_final_revalidation_refuses_blocker_added_during_stack_inspection() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let count = temp.path().join("pr-view-count");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    current=0
    test ! -f '{}' || current=$(cat '{}')
    current=$((current + 1))
    printf '%s' "$current" > '{}'
    if test "$current" -eq 1; then
      printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}'
    else
      printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"5·UnReSoLvEd"}}],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}'
    fi ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display(),
            count.display(),
            count.display(),
            count.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_final_live_revalidation")
    );
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert_eq!(calls.matches("pr view 42").count(), 2, "{calls}");
    assert!(calls.contains("stackConfig"), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn steward_refuses_formal_stack_before_enqueue_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"stackConfig"*)
    printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":{{"number":7,"size":3,"baseRefName":"main"}},"stackEntry":{{"position":2}}}}}}}}}}' ;;
  *"stackEntry"*)
    printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":{{"number":7,"size":3,"baseRefName":"main"}},"stackEntry":{{"position":2}}}}}}}}}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    observation.base = "layer-one".to_owned();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let policy = queue_policy();
    let (mutation, error) = enqueue_pull_request(&context, &pr, &policy, &mut ledger);

    assert!(mutation.is_none());
    assert!(
        error.as_deref().is_some_and(
            |message| message.contains("position 2/3 in GitHub stack #7")
                && message.contains("\"mode\":\"observe\"")
                && message.contains("\"github_mutation\":false")
                && message.contains("\"required_checks_suppressed\":false")
        ),
        "{error:?}"
    );
    let calls = fs::read_to_string(log).expect("calls");
    assert!(calls.contains("stackEntry"), "{calls}");
    assert!(
        calls.contains("config=main:.shipyard/config.toml"),
        "{calls}"
    );
    assert!(!calls.contains("config=layer-one:"), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
    let state_root = mutation_control.store.path().parent().expect("state root");
    assert!(
        crate::merge_queue_control::uncertain_mutations(state_root)
            .expect("uncertainty")
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn steward_apply_rejects_unauthorized_host_before_remote_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "m1");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("authority is `studio`")),
        "{error:?}"
    );
    let calls = fs::read_to_string(log).expect("calls");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn unauthorized_steward_does_not_consume_transient_rerun_budget() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"TIMED_OUT","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"rerun-failed-jobs"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let mut pr = ready_pr();
    pr.fact.checks[0].conclusion = Some("TIMED_OUT".to_owned());
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "m1");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::RerunTransient { run_ids: vec![100] },
        &mut ledger,
    );

    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("authority is `studio`")),
        "{error:?}"
    );
    assert!(ledger.transient_attempts.is_empty());
    let calls = fs::read_to_string(log).expect("calls");
    assert!(!calls.contains("rerun-failed-jobs"), "{calls}");
}

#[test]
fn known_unperformed_transient_rerun_does_not_consume_budget() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger_path = temp.path().join("ledger.json");
    let key = "owner/repo#42:head:100".to_owned();
    let mut ledger = StewardLedger {
        transient_attempts: BTreeMap::from([(key.clone(), 1)]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("seed intent");

    rollback_transient_attempt(
        &mut ledger,
        &ledger_path,
        &key,
        "owner/repo",
        100,
        "head",
        "rerun_transient_skipped_after_live_revalidation",
    )
    .expect("rollback");

    assert!(!ledger.transient_attempts.contains_key(&key));
    assert!(
        !load_ledger(&ledger_path)
            .expect("reload")
            .transient_attempts
            .contains_key(&key)
    );
}

#[test]
fn unauthorized_steward_does_not_consume_capacity_preemption_budget() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = GitHubActions::new(temp.path());
    let mut pr = ready_pr();
    pr.fact.queue_position = Some(1);
    let mut observation = observation_for(pr, true);
    observation.merge_group_heads.insert(42, "b".repeat(40));
    observation.merge_group_enqueued_at.insert(
        42,
        (Utc::now() - chrono::Duration::minutes(20)).to_rfc3339(),
    );
    observation.runs = vec![queued_run(100, "2026-07-26T00:00:00Z")];
    let cancellation = RunCancellation {
        run_id: 100,
        reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
    };
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "m1");
    let ledger_path = temp.path().join("ledger.json");
    let context = CapacityApplyContext {
        actions: &actions,
        observation: &observation,
        cancellation: &cancellation,
        ledger_path: &ledger_path,
        mutation_control: &mutation_control,
        managed_label: MANAGED_LABEL,
        handoff_context: HANDOFF_CONTEXT,
        provenance_blocking_labels: &[],
    };

    let (mutation, error) = apply_capacity_preemption(&context, "steward:skip", &mut ledger);

    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("authority is `studio`")),
        "{error:?}"
    );
    assert!(ledger.preemption_attempts.is_empty());
    assert!(ledger.audit.is_empty());
}

#[test]
fn capacity_revalidation_rejects_a_new_workflow_attempt() {
    let observed = queued_run(100, "2026-07-26T00:00:00Z");
    let mut rerun = observed.clone();
    rerun.run_attempt += 1;

    assert!(!same_workflow_attempt(&observed, &rerun));
}

#[cfg(unix)]
#[test]
fn initial_force_cancel_revalidation_failure_is_durably_rejected_before_post() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = mismatched_force_cancel_identity(&temp, &calls);
    let control = mutation_control(&temp, "studio", "studio");
    let pending = pending_cancellation_record();
    let key = pending_cancellation_key(&pending);
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger {
        pending_cancellations: BTreeMap::from([(key, pending.clone())]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("seed ledger");
    let observation = observation_for(ready_pr(), true);
    let cancellation = RunCancellation {
        run_id: 100,
        reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
    };
    let context = CapacityApplyContext {
        actions: &actions,
        observation: &observation,
        cancellation: &cancellation,
        ledger_path: &ledger_path,
        mutation_control: &control,
        managed_label: MANAGED_LABEL,
        handoff_context: HANDOFF_CONTEXT,
        provenance_blocking_labels: &[],
    };
    let active = NonTerminalRun {
        status: "in_progress".to_owned(),
        jobs: Vec::new(),
    };

    let (mutation, error) =
        force_cancel_nonterminal_run(&context, pending.run_id, &active, &mut ledger);

    assert_eq!(mutation.as_deref(), Some("cancel_not_terminal"));
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("revalidation failed")),
        "{error:?}"
    );
    assert_force_cancel_revalidation_rejected(
        &calls,
        &control,
        &ledger_path,
        &ledger,
        "force_cancel_revalidation_failed",
    );
}

#[cfg(unix)]
#[test]
fn recovered_force_cancel_revalidation_failure_is_durably_rejected_before_post() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = mismatched_force_cancel_identity(&temp, &calls);
    let control = mutation_control(&temp, "studio", "studio");
    let pending = pending_cancellation_record();
    let key = pending_cancellation_key(&pending);
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger {
        pending_cancellations: BTreeMap::from([(key.clone(), pending.clone())]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("seed ledger");
    let active = NonTerminalRun {
        status: "in_progress".to_owned(),
        jobs: Vec::new(),
    };

    let error = resume_force_cancel_after_normal_wait(
        &actions,
        &ledger_path,
        &mut ledger,
        &control,
        &key,
        &pending,
        &active,
    )
    .expect_err("identity drift must reject force-cancel");

    assert!(error.contains("revalidation failed"), "{error}");
    assert_force_cancel_revalidation_rejected(
        &calls,
        &control,
        &ledger_path,
        &ledger,
        "pending_force_cancel_revalidation_failed",
    );
}

#[cfg(unix)]
#[test]
fn blocker_added_after_normal_cancel_acceptance_prevents_force_cancel() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = provenance_blocked_force_cancel(&temp, &calls);
    let control = mutation_control(&temp, "studio", "studio");
    let pending = pending_cancellation_record();
    let key = pending_cancellation_key(&pending);
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger {
        pending_cancellations: BTreeMap::from([(key, pending.clone())]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("seed ledger");
    let observation = observation_for(ready_pr(), true);
    let cancellation = RunCancellation {
        run_id: pending.run_id,
        reason: RunCancellationReason::AdvisoryPreambleCapacityTheft,
    };
    let context = CapacityApplyContext {
        actions: &actions,
        observation: &observation,
        cancellation: &cancellation,
        ledger_path: &ledger_path,
        mutation_control: &control,
        managed_label: MANAGED_LABEL,
        handoff_context: HANDOFF_CONTEXT,
        provenance_blocking_labels: &pending.provenance_blocking_labels,
    };
    let active = NonTerminalRun {
        status: "in_progress".to_owned(),
        jobs: Vec::new(),
    };

    let (mutation, error) =
        force_cancel_nonterminal_run(&context, pending.run_id, &active, &mut ledger);

    assert_eq!(mutation.as_deref(), Some("cancel_not_terminal"));
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("provenance-blocking label")),
        "{error:?}"
    );
    assert_force_cancel_revalidation_rejected(
        &calls,
        &control,
        &ledger_path,
        &ledger,
        "force_cancel_revalidation_failed",
    );
}

#[cfg(unix)]
#[test]
fn blocker_added_before_restart_prevents_recovered_force_cancel() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let actions = provenance_blocked_force_cancel(&temp, &calls);
    let control = mutation_control(&temp, "studio", "studio");
    let pending = pending_cancellation_record();
    let key = pending_cancellation_key(&pending);
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger {
        pending_cancellations: BTreeMap::from([(key.clone(), pending.clone())]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("seed ledger");
    let active = NonTerminalRun {
        status: "in_progress".to_owned(),
        jobs: Vec::new(),
    };

    let error = resume_force_cancel_after_normal_wait(
        &actions,
        &ledger_path,
        &mut ledger,
        &control,
        &key,
        &pending,
        &active,
    )
    .expect_err("late provenance blocker must reject recovered force-cancel");

    assert!(error.contains("provenance-blocking label"), "{error}");
    assert_force_cancel_revalidation_rejected(
        &calls,
        &control,
        &ledger_path,
        &ledger,
        "pending_force_cancel_revalidation_failed",
    );
}

#[cfg(unix)]
fn provenance_blocked_force_cancel(temp: &tempfile::TempDir, calls: &Path) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view 42 "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature-current","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}},{{"name":"5·UnReSoLvEd"}}],"statusCheckRollup":[{{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-22T00:00:00Z"}}]}}' ;;
  *"/force-cancel") echo "force-cancel POST must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls.display()
        ),
    )
}

#[cfg(unix)]
fn mismatched_force_cancel_identity(temp: &tempfile::TempDir, calls: &Path) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view 42 "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature-current","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}}],"statusCheckRollup":[{{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-22T00:00:00Z"}}]}}' ;;
  "api repos/owner/repo/actions/runs/100")
    printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":2,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"in_progress","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *"/force-cancel") echo "force-cancel POST must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls.display()
        ),
    )
}

#[cfg(unix)]
fn assert_force_cancel_revalidation_rejected(
    calls: &Path,
    control: &MutationControl,
    ledger_path: &Path,
    ledger: &StewardLedger,
    expected_action: &str,
) {
    let calls = fs::read_to_string(calls).expect("calls");
    assert!(!calls.contains("/force-cancel"), "{calls}");
    let pending = ledger
        .pending_cancellations
        .values()
        .next()
        .expect("pending force-cancel");
    assert!(
        !DurableMutationIntent::resume(&pending.mutation_correlation_id)
            .expect("durable intent")
            .is_uncertain(control.store.path().parent().expect("state root"))
            .expect("uncertainty")
    );
    assert!(
        crate::merge_queue_control::uncertain_mutations(
            control.store.path().parent().expect("state root")
        )
        .expect("global uncertainty")
        .is_empty()
    );
    assert!(
        ledger
            .audit
            .iter()
            .any(|entry| entry.action == expected_action),
        "missing {expected_action}"
    );
    let persisted = load_ledger(ledger_path).expect("persisted rejection audit");
    assert!(
        persisted
            .audit
            .iter()
            .any(|entry| entry.action == expected_action),
        "missing persisted {expected_action}"
    );
}

#[test]
fn initial_capacity_guard_correlation_is_durable_before_started_audit_can_be_orphaned() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = GitHubActions::new(temp.path());
    let mut observation = observation_for(ready_pr(), true);
    let run = queued_run(100, "2026-07-26T00:00:00Z");
    observation.runs = vec![run.clone()];
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
        managed_label: MANAGED_LABEL,
        handoff_context: HANDOFF_CONTEXT,
        provenance_blocking_labels: &[],
    };
    let mut ledger = StewardLedger::default();

    let (guard, pending) = start_capacity_preemption(
        &context,
        "steward:skip",
        &mut ledger,
        &run,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .expect("durable guard start");
    let persisted = load_ledger(&ledger_path).expect("persisted ledger");
    assert_eq!(
        persisted
            .pending_cancellations
            .get(&pending_cancellation_key(&pending))
            .expect("pending before crash")
            .mutation_correlation_id,
        guard.correlation_id()
    );

    drop(guard);

    assert!(
        DurableMutationIntent::resume(&pending.mutation_correlation_id)
            .expect("durable intent")
            .is_uncertain(control.store.path().parent().expect("state root"))
            .expect("uncertainty")
    );
}

#[test]
fn skipped_capacity_intent_recovers_without_sending_a_cancellation() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = GitHubActions::new(temp.path());
    let mut observation = observation_for(ready_pr(), true);
    let run = queued_run(100, "2026-07-26T00:00:00Z");
    observation.runs = vec![run.clone()];
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
        managed_label: MANAGED_LABEL,
        handoff_context: HANDOFF_CONTEXT,
        provenance_blocking_labels: &[],
    };
    let mut ledger = StewardLedger::default();
    let (guard, pending) = start_capacity_preemption(
        &context,
        "steward:skip",
        &mut ledger,
        &run,
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    )
    .expect("durable guard start");
    let key = pending_cancellation_key(&pending);
    mark_cancellation_skipped(&mut ledger, &ledger_path, &key).expect("durable tombstone");

    drop(guard);

    let mut recovered = load_ledger(&ledger_path).expect("persisted tombstone");
    let tombstone = recovered
        .pending_cancellations
        .get(&key)
        .expect("skipped pending")
        .clone();
    assert_eq!(tombstone.phase, PendingCancellationPhase::Skipped);
    assert_eq!(
        resume_pending_cancellation(
            &actions,
            &ledger_path,
            &mut recovered,
            &control,
            &key,
            &tombstone,
        )
        .expect("skip recovery"),
        "recovered_skipped_cancellation"
    );
    assert!(recovered.pending_cancellations.is_empty());
    assert!(
        !DurableMutationIntent::resume(&tombstone.mutation_correlation_id)
            .expect("durable intent")
            .is_uncertain(control.store.path().parent().expect("state root"))
            .expect("uncertainty")
    );
}

#[cfg(unix)]
#[test]
fn post_accepted_crash_does_not_clear_uncertain_intent_when_evidence_disappears() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"actions/runs/100/cancel") exit 0 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let control = mutation_control(&temp, "studio", "studio");
    let mut pending = pending_cancellation_record();
    pending.phase = PendingCancellationPhase::Intent;
    let intent = DurableMutationIntent::new();
    let guard = acquire_pending_cancellation_guard(
        &control,
        &pending,
        "runner steward preempt capacity run 100",
        &intent,
    )
    .expect("guard");
    intent
        .correlation_id()
        .clone_into(&mut pending.mutation_correlation_id);
    let key = pending_cancellation_key(&pending);
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = StewardLedger {
        pending_cancellations: BTreeMap::from([(key.clone(), pending.clone())]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("intent persisted");

    actions
        .cancel_workflow_run(&pending.repo, pending.run_id)
        .expect("POST accepted");
    drop(guard);

    assert!(pending_uncertainty(&control, &pending).expect("uncertain"));
    let error =
        resolve_rejected_pending_intent(&mut ledger, &ledger_path, &control, &key, &pending, true)
            .expect_err("uncertain POST cannot become skipped");
    assert!(error.contains("preserving pending state"), "{error}");
    assert_eq!(
        ledger
            .pending_cancellations
            .get(&key)
            .expect("pending preserved")
            .phase,
        PendingCancellationPhase::Intent
    );
    assert!(
        load_ledger(&ledger_path)
            .expect("reload")
            .pending_cancellations
            .contains_key(&key)
    );
    assert!(pending_uncertainty(&control, &pending).expect("uncertainty preserved"));
}

#[cfg(unix)]
#[test]
fn pending_capacity_cancellation_resumes_after_cancel_accepted_restart() {
    let temp = tempfile::tempdir().expect("temp");
    let calls = temp.path().join("calls");
    let reads = temp.path().join("reads");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view 42 "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature-current","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}}],"statusCheckRollup":[{{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-22T00:00:00Z"}}]}}' ;;
  *"/force-cancel") exit 0 ;;
  *"/jobs?"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100/attempts/1"|*"actions/runs/100")
    count=0
    test ! -f '{}' || count=$(cat '{}')
    count=$((count + 1))
    printf '%s' "$count" > '{}'
    if test "$count" -le 5; then status=in_progress; else status=completed; fi
    printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            calls.display(),
            reads.display(),
            reads.display(),
            reads.display(),
        ),
    );
    let control = mutation_control(&temp, "studio", "studio");
    let mut pending = pending_cancellation_record();
    let intent = DurableMutationIntent::new();
    let interrupted_guard = acquire_pending_cancellation_guard(
        &control,
        &pending,
        "runner steward preempt capacity run 100",
        &intent,
    )
    .expect("interrupted guard");
    interrupted_guard
        .correlation_id()
        .clone_into(&mut pending.mutation_correlation_id);
    drop(interrupted_guard);
    let key = pending_cancellation_key(&pending);
    let mut ledger = StewardLedger {
        preemption_attempts: BTreeMap::from([(
            "owner/repo:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            1,
        )]),
        pending_cancellations: BTreeMap::from([(key, pending.clone())]),
        ..StewardLedger::default()
    };
    let ledger_path = temp.path().join("ledger.json");
    save_ledger(&ledger_path, &ledger).expect("seed ledger");

    let (errors, cancellations) =
        resume_pending_cancellations(&actions, &ledger_path, &mut ledger, &control);

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(cancellations["owner/repo"][0].run_id, 100);
    assert!(ledger.pending_cancellations.is_empty());
    assert_eq!(ledger.preemption_attempts.values().copied().sum::<u32>(), 1);
    let calls = fs::read_to_string(calls).expect("calls");
    assert!(calls.contains("/force-cancel"), "{calls}");
    let reloaded = load_ledger(&ledger_path).expect("reload");
    assert!(reloaded.pending_cancellations.is_empty());
    assert!(
        !DurableMutationIntent::resume(&pending.mutation_correlation_id)
            .expect("durable intent")
            .is_uncertain(control.store.path().parent().expect("state root"))
            .expect("uncertainties")
    );
}

#[cfg(unix)]
#[test]
fn pending_cancellation_survives_transient_read_failure_then_recovers() {
    let temp = tempfile::tempdir().expect("temp");
    let ledger_path = temp.path().join("ledger.json");
    let pending = pending_cancellation_record();
    let key = pending_cancellation_key(&pending);
    let mut ledger = StewardLedger {
        pending_cancellations: BTreeMap::from([(key.clone(), pending)]),
        ..StewardLedger::default()
    };
    save_ledger(&ledger_path, &ledger).expect("seed ledger");
    let control = mutation_control(&temp, "studio", "studio");
    let failing = fake_gh(&temp, r#"echo "temporary read failure" >&2; exit 1"#);

    let (errors, cancellations) =
        resume_pending_cancellations(&failing, &ledger_path, &mut ledger, &control);

    assert_eq!(errors.len(), 1);
    assert!(cancellations.is_empty());
    assert!(ledger.pending_cancellations.contains_key(&key));
    assert!(
        load_ledger(&ledger_path)
            .expect("reload failed recovery")
            .pending_cancellations
            .contains_key(&key)
    );

    let reads = temp.path().join("reads");
    let recovered = fake_gh(
        &temp,
        &format!(
            r#"
case "$*" in
  "pr view 42 "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature-current","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{{"name":"shipyard:managed"}}],"statusCheckRollup":[{{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-22T00:00:00Z"}}]}}' ;;
  *"/force-cancel") exit 0 ;;
  *"/jobs?"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100/attempts/1"|*"actions/runs/100")
    count=0
    test ! -f '{}' || count=$(cat '{}')
    count=$((count + 1))
    printf '%s' "$count" > '{}'
    if test "$count" -le 5; then status=in_progress; else status=completed; fi
    printf '%s' '{{"id":100,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            reads.display(),
            reads.display(),
            reads.display(),
        ),
    );

    let (errors, cancellations) =
        resume_pending_cancellations(&recovered, &ledger_path, &mut ledger, &control);

    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(cancellations["owner/repo"][0].run_id, 100);
    assert!(ledger.pending_cancellations.is_empty());
}

#[cfg(unix)]
#[test]
fn steward_apply_obeys_central_hold_before_remote_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"enqueuePullRequest"*) echo "mutation must not run" >&2; exit 90 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let state_root = mutation_control.store.path().parent().expect("state root");
    crate::merge_queue_control::hold(state_root, "incident").expect("hold");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("centrally held")),
        "{error:?}"
    );
    let calls = fs::read_to_string(log).expect("calls");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn steward_ambiguous_failure_is_durable_shared_uncertainty() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":null,"pullRequest":{{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) echo "connection reset after request body" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let first = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    assert!(first.0.is_none());
    assert!(first.1.is_some());
    let state_root = mutation_control.store.path().parent().expect("state root");
    let uncertain =
        crate::merge_queue_control::uncertain_mutations(state_root).expect("uncertainty");
    assert_eq!(uncertain.len(), 1);
    assert_eq!(
        uncertain[0]["action"],
        "runner steward enqueue pull request"
    );
    assert_eq!(uncertain[0]["pr"], 42);

    let second = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    assert!(
        second
            .1
            .as_deref()
            .is_some_and(|message| message.contains("is uncertain")),
        "{second:?}"
    );
    let calls = fs::read_to_string(log).expect("calls");
    assert_eq!(calls.matches("enqueuePullRequest").count(), 1, "{calls}");
}

#[cfg(unix)]
#[test]
fn steward_dry_run_needs_no_mutation_authority_and_makes_no_remote_write() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, r#"echo "unexpected mutation: $*" >&2; exit 90"#);
    let pr = ready_pr();
    let observation = observation_for(pr, true);
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: true,
        preempt_capacity: true,
        max_preemptions_per_head: 1,
        apply: false,
        ledger: None,
    };
    let mut ledger = StewardLedger::default();

    let (reports, failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &queue_policy(),
        &temp.path().join("ledger.json"),
        &mut ledger,
        None,
    );

    assert!(!failed);
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].decision, StewardDecision::ArmMergeQueue);
    assert!(reports[0].mutation.is_none());
    assert!(reports[0].error.is_none());
    assert!(!temp.path().join("state/ship").exists());
}

#[cfg(unix)]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the exact read-only GitHub transcript is intentionally kept beside its scenario"
)]
fn provenance_blocker_with_opt_out_and_steward_state_makes_no_github_write() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
calls_path=$(dirname "$0")/calls
queue_query='query=query($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100,after:$cursor){nodes{position enqueuedAt headCommit{oid} pullRequest{number}} pageInfo{hasNextPage endCursor}}}}}'
pr_fields='id,number,state,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup'
if test "$#" -eq 10 \
  && test "$1" = api \
  && test "$2" = graphql \
  && test "$3" = -f \
  && test "$4" = "$queue_query" \
  && test "$5" = -F \
  && test "$6" = owner=owner \
  && test "$7" = -F \
  && test "$8" = name=repo \
  && test "$9" = -F \
  && test "${10}" = branch=main
then
  printf '%s:%s\n' "$#" "$*" >> "$calls_path"
  printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}'
elif test "$#" -eq 7 \
  && test "$1" = pr \
  && test "$2" = view \
  && test "$3" = 42 \
  && test "$4" = --repo \
  && test "$5" = owner/repo \
  && test "$6" = --json \
  && test "$7" = "$pr_fields"
then
  printf '%s:%s\n' "$#" "$*" >> "$calls_path"
  printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[{"name":"shipyard:no-auto-merge"},{"name":"5·UNRESOLVED"},{"name":"shipyard:managed"},{"name":"shipyard:unmanaged"},{"name":"shipyard:needs-agent"}],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-07-26T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"},{"__typename":"StatusContext","context":"shipyard/steward-handoff","state":"SUCCESS","createdAt":"2026-08-22T00:00:00Z"},{"__typename":"StatusContext","context":"shipyard/recovery","state":"FAILURE","createdAt":"2026-08-22T00:00:01Z"}]}'
else
  echo "unexpected GitHub invocation argc=$# argv=$*" >&2
  exit 90
fi
"#,
    );
    let mut pr = ready_pr();
    pr.fact.labels.extend([
        "shipyard:no-auto-merge".to_owned(),
        "5·UNRESOLVED".to_owned(),
        MANAGED_LABEL.to_owned(),
        UNMANAGED_LABEL.to_owned(),
        NEEDS_AGENT_LABEL.to_owned(),
    ]);
    pr.fact.checks.extend([
        StewardCheck {
            name: HANDOFF_CONTEXT.to_owned(),
            source: StewardCheckSource::StatusContext,
            app_id: None,
            check_run_id: None,
            status: "COMPLETED".to_owned(),
            conclusion: Some("SUCCESS".to_owned()),
            run_id: None,
            observed_at: Some("2026-08-22T00:00:00Z".to_owned()),
        },
        StewardCheck {
            name: RECOVERY_CONTEXT.to_owned(),
            source: StewardCheckSource::StatusContext,
            app_id: None,
            check_run_id: None,
            status: "COMPLETED".to_owned(),
            conclusion: Some("FAILURE".to_owned()),
            run_id: None,
            observed_at: Some("2026-08-22T00:00:01Z".to_owned()),
        },
    ]);
    let observation = observation_for(pr, true);
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "shipyard:no-auto-merge".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: true,
        preempt_capacity: true,
        max_preemptions_per_head: 1,
        apply: true,
        ledger: None,
    };
    let control = mutation_control(&temp, "studio", "studio");
    let mut policy = queue_policy();
    policy.opt_out_label = "shipyard:no-auto-merge".to_owned();
    let (reports, failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &policy,
        &temp.path().join("ledger.json"),
        &mut StewardLedger::default(),
        Some(&control),
    );

    assert!(!failed, "{reports:?}");
    assert_eq!(
        serde_json::to_value(&reports[0].decision).expect("serialize"),
        serde_json::json!({"action":"provenance_blocked","labels":["5·unresolved"]})
    );
    assert!(reports[0].mutation.is_none());
    assert!(reports[0].error.is_none());
    let calls = fs::read_to_string(temp.path().join("calls")).expect("revalidation calls");
    assert_eq!(
        calls,
        concat!(
            "10:api graphql -f query=query($owner:String!,$name:String!,$branch:String!,$cursor:String){repository(owner:$owner,name:$name){mergeQueue(branch:$branch){entries(first:100,after:$cursor){nodes{position enqueuedAt headCommit{oid} pullRequest{number}} pageInfo{hasNextPage endCursor}}}}} -F owner=owner -F name=repo -F branch=main\n",
            "7:pr view 42 --repo owner/repo --json id,number,state,isDraft,baseRefName,headRefOid,headRefName,mergeStateStatus,autoMergeRequest,labels,statusCheckRollup\n",
        ),
        "only the ordered fenced revalidation reads are allowed"
    );
}

#[cfg(unix)]
#[test]
fn routing_readiness_hold_does_not_suppress_an_unrelated_pr_in_repo_plan() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, r#"echo "unexpected mutation: $*" >&2; exit 90"#);
    let mut held = ready_pr();
    held.node_id = "PR_held".to_owned();
    held.fact.number = 41;
    held.fact.labels.push("steward:skip".to_owned());
    let eligible = ready_pr();
    let mut observation = observation_for(held, true);
    observation.prs.push(eligible);
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: true,
        preempt_capacity: true,
        max_preemptions_per_head: 1,
        apply: false,
        ledger: None,
    };

    let (reports, failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &queue_policy(),
        &temp.path().join("ledger.json"),
        &mut StewardLedger::default(),
        None,
    );

    assert!(!failed);
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].number, 41);
    assert_eq!(reports[0].decision, StewardDecision::OptedOut);
    assert_eq!(reports[1].number, 42);
    assert_eq!(reports[1].decision, StewardDecision::ArmMergeQueue);
}

#[test]
fn disabled_preemption_ignores_preemption_only_observation_errors() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = GitHubActions::new(".");
    let pr = ready_pr();
    let mut observation = observation_for(pr, true);
    observation.preemption_error = Some("preemption job hydration failed".to_owned());
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: false,
        preempt_capacity: false,
        max_preemptions_per_head: 1,
        apply: false,
        ledger: None,
    };
    let mut ledger = StewardLedger::default();

    let (report, failed, planned) = apply_repo_plan(
        &actions,
        &args,
        &observation,
        &temp.path().join("ledger.json"),
        &mut ledger,
        1,
        None,
    );

    assert!(!failed);
    assert_eq!(planned, 0);
    assert!(report.errors.is_empty());
}

#[test]
fn direct_mode_refusal_never_reaches_rest_merge_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = GitHubActions::new(".");
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), false);
    let mut policy = queue_policy();
    policy.merge_queue = false;
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    assert!(matches!(
        decision,
        StewardDecision::DirectMergeRefused { ref reasons }
            if reasons.contains(
                &crate::merge_steward::DirectMergeRefusal::ValidatedBaseRevisionNotAtomic
            )
    ));
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(&context, &pr, &policy, &decision, &mut ledger);

    assert!(mutation.is_none());
    assert!(error.is_none());
    assert!(ledger.audit.is_empty());
}

#[cfg(unix)]
#[test]
fn enqueue_skips_when_pr_is_retargeted_between_observation_and_apply() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"release","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS"}}]}}' ;;
  *"enqueuePullRequest"*) echo "unexpected mutation" >&2; exit 2 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert_eq!(mutation.as_deref(), Some("skipped_after_live_revalidation"));
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert!(calls.contains("pr view 42"), "{calls}");
    assert!(!calls.contains("enqueuePullRequest"), "{calls}");
}

#[cfg(unix)]
#[test]
fn enqueue_requirements_refusal_is_waiting_not_control_plane_failure() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}]}' ;;
  *"stackConfig"*) printf '%s' '{"data":{"repository":{"stackConfig":null,"pullRequest":{"headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","stack":null,"stackEntry":null}}}}' ;;
  *"stackEntry"*) printf '%s' '{"data":{"repository":{"pullRequest":{"stack":null,"stackEntry":null}}}}' ;;
  *"enqueuePullRequest"*)
    echo "Required approving review has not been submitted" >&2
    exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let mut pr = ready_pr();
    pr.fact.merge_state = "BLOCKED".to_owned();
    let observation = observation_for(pr.clone(), true);
    let policy = queue_policy();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );

    assert_eq!(mutation.as_deref(), Some("waiting_enqueue_requirements"));
    assert!(error.is_none(), "{error:?}");
    assert_eq!(ledger.terminal_handoffs.len(), 1);
    assert_eq!(
        ledger
            .terminal_handoffs
            .values()
            .next()
            .expect("resolved continuation")
            .phase,
        TerminalHandoffPhase::Resolved
    );
    assert!(!enqueue_requirements_pending(
        "HTTP 403: Resource not accessible by integration: required review"
    ));
}

#[cfg(unix)]
#[test]
fn legacy_same_head_duplicate_reason_is_rejected_before_github_reads() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            "printf '%s\n' \"$*\" >> '{}'\necho 'unexpected GitHub call' >&2\nexit 2",
            log.display()
        ),
    );
    let pr = ready_pr();
    let mut observation = observation_for(pr, true);
    observation.runs = vec![
        queued_run(1, "2026-07-26T00:00:00Z"),
        queued_run(2, "2026-07-26T01:00:00Z"),
    ];
    let cancellation = RunCancellation {
        run_id: 1,
        reason: RunCancellationReason::DuplicateImmutableHead,
    };
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");

    let (mutation, error) = apply_run_cancellation(
        &actions,
        &observation,
        &cancellation,
        "steward:skip",
        MANAGED_LABEL,
        HANDOFF_CONTEXT,
        &[],
        &temp.path().join("ledger.json"),
        &mut ledger,
        &mutation_control,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_non_authorizing_cancellation_reason")
    );
    assert!(error.is_none(), "{error:?}");
    assert!(!log.exists(), "legacy reason must not reach GitHub");
}

#[cfg(unix)]
#[test]
fn superseded_head_cancel_transport_reproves_exact_run_before_mutation() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *"actions/runs/1/cancel"*) printf '%s' '{{}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display()
        ),
    );
    let mut pr = ready_pr();
    pr.fact.head_sha = "b".repeat(40);
    let mut observation = observation_for(pr, true);
    observation.runs = vec![queued_run(1, "2026-07-26T00:00:00Z")];
    let cancellation = RunCancellation {
        run_id: 1,
        reason: RunCancellationReason::SupersededPullRequestHead,
    };
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");

    let (mutation, error) = apply_run_cancellation(
        &actions,
        &observation,
        &cancellation,
        "steward:skip",
        MANAGED_LABEL,
        HANDOFF_CONTEXT,
        &[],
        &temp.path().join("ledger.json"),
        &mut ledger,
        &mutation_control,
    );

    assert_eq!(mutation.as_deref(), Some("cancelled"));
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert_eq!(calls.matches("pr view 42").count(), 3, "{calls}");
    let exact = calls
        .find("api repos/owner/repo/actions/runs/1\n")
        .expect("exact run re-read");
    let cancel = calls
        .find("api -X POST repos/owner/repo/actions/runs/1/cancel")
        .expect("cancel call");
    assert!(exact < cancel, "{calls}");
}

#[cfg(unix)]
#[test]
fn superseded_head_cancel_is_rejected_if_head_returns_before_final_guarded_check() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view "*)
    if [ "$(grep -c '^pr view ' '{}')" -le 2 ]; then
      head="bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    else
      head="aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    fi
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"'"$head"'","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *"actions/runs/1/cancel"*) echo "unexpected cancel" >&2; exit 2 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display(),
            log.display()
        ),
    );
    let mut pr = ready_pr();
    pr.fact.head_sha = "b".repeat(40);
    let mut observation = observation_for(pr, true);
    observation.runs = vec![queued_run(1, "2026-07-26T00:00:00Z")];
    let cancellation = RunCancellation {
        run_id: 1,
        reason: RunCancellationReason::SupersededPullRequestHead,
    };
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");

    let (mutation, error) = apply_run_cancellation(
        &actions,
        &observation,
        &cancellation,
        "steward:skip",
        MANAGED_LABEL,
        HANDOFF_CONTEXT,
        &[],
        &temp.path().join("ledger.json"),
        &mut ledger,
        &mutation_control,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_final_authority_check")
    );
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert_eq!(calls.matches("pr view 42").count(), 3, "{calls}");
    assert!(!calls.contains("actions/runs/1/cancel"), "{calls}");
}

#[cfg(unix)]
#[test]
fn superseded_head_cancel_is_rejected_if_exact_run_starts_after_authority_read() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    if [ "$(grep -c '^api repos/owner/repo/actions/runs/1$' '{}')" -le 2 ]; then
      status="queued"
    else
      status="in_progress"
    fi
    printf '%s' '{{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
  *"actions/runs/1/cancel"*) echo "unexpected cancel" >&2; exit 2 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log.display(),
            log.display()
        ),
    );
    let mut pr = ready_pr();
    pr.fact.head_sha = "b".repeat(40);
    let mut observation = observation_for(pr, true);
    observation.runs = vec![queued_run(1, "2026-07-26T00:00:00Z")];
    let cancellation = RunCancellation {
        run_id: 1,
        reason: RunCancellationReason::SupersededPullRequestHead,
    };
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");

    let (mutation, error) = apply_run_cancellation(
        &actions,
        &observation,
        &cancellation,
        "steward:skip",
        MANAGED_LABEL,
        HANDOFF_CONTEXT,
        &[],
        &temp.path().join("ledger.json"),
        &mut ledger,
        &mutation_control,
    );

    assert_eq!(
        mutation.as_deref(),
        Some("skipped_after_final_exact_run_check")
    );
    assert!(error.is_none(), "{error:?}");
    let calls = fs::read_to_string(log).expect("calls");
    assert_eq!(
        calls
            .matches("api repos/owner/repo/actions/runs/1\n")
            .count(),
        3,
        "{calls}"
    );
    assert!(!calls.contains("actions/runs/1/cancel"), "{calls}");
}

#[cfg(unix)]
#[test]
fn definitive_superseded_cancel_rejection_finishes_guard_without_uncertainty() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  "pr view "*)
    printf '%s' '{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[]}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":null}}}' ;;
  *"actions/runs?status=queued"*)
    printf '%s' '{"workflow_runs":[{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}]}' ;;
  *"actions/runs?status="*) printf '%s' '{"workflow_runs":[]}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{"id":1,"workflow_id":77,"run_attempt":1,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}' ;;
  *"actions/runs/1/cancel"*) echo "HTTP 422: Cannot cancel a completed workflow run" >&2; exit 1 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let mut pr = ready_pr();
    pr.fact.head_sha = "b".repeat(40);
    let mut observation = observation_for(pr, true);
    observation.runs = vec![queued_run(1, "2026-07-26T00:00:00Z")];
    let cancellation = RunCancellation {
        run_id: 1,
        reason: RunCancellationReason::SupersededPullRequestHead,
    };
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");

    let (mutation, error) = apply_run_cancellation(
        &actions,
        &observation,
        &cancellation,
        "steward:skip",
        MANAGED_LABEL,
        HANDOFF_CONTEXT,
        &[],
        &temp.path().join("ledger.json"),
        &mut ledger,
        &mutation_control,
    );

    assert_eq!(mutation.as_deref(), Some("cancel_rejected"));
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("HTTP 422")),
        "{error:?}"
    );
    let state_root = mutation_control.store.path().parent().expect("state root");
    assert!(
        crate::merge_queue_control::uncertain_mutations(state_root)
            .expect("uncertainties")
            .is_empty()
    );
}
