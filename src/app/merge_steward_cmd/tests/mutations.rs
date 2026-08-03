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
#[cfg(unix)]
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
use crate::app::merge_steward_cmd::pr_mutations::rollback_transient_attempt;

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
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);

    let (mutation, error) = enqueue_pull_request(&context, &pr, &mut ledger);

    assert!(mutation.is_none());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("position 2/3 in GitHub stack #7")),
        "{error:?}"
    );
    let calls = fs::read_to_string(log).expect("calls");
    assert!(calls.contains("stackEntry"), "{calls}");
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
fn mismatched_force_cancel_identity(temp: &tempfile::TempDir, calls: &Path) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{}'
case "$*" in
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
  *"/force-cancel") exit 0 ;;
  *"/jobs?"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100/attempts/1"|*"actions/runs/100")
    count=0
    test ! -f '{}' || count=$(cat '{}')
    count=$((count + 1))
    printf '%s' "$count" > '{}'
    if test "$count" -le 5; then status=in_progress; else status=completed; fi
    printf '%s' '{{"id":100,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
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
  *"/force-cancel") exit 0 ;;
  *"/jobs?"*) printf '%s' '{{"jobs":[]}}' ;;
  *"actions/runs/100/attempts/1"|*"actions/runs/100")
    count=0
    test ! -f '{}' || count=$(cat '{}')
    count=$((count + 1))
    printf '%s' "$count" > '{}'
    if test "$count" -le 5; then status=in_progress; else status=completed; fi
    printf '%s' '{{"id":100,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
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
        max_transient_reruns: 1,
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
        max_transient_reruns: 1,
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
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
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
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
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
    printf '%s' '{{"workflow_runs":[{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}]}}' ;;
  *"actions/runs?status="*) printf '%s' '{{"workflow_runs":[]}}' ;;
  "api repos/owner/repo/actions/runs/1")
    if [ "$(grep -c '^api repos/owner/repo/actions/runs/1$' '{}')" -le 2 ]; then
      status="queued"
    else
      status="in_progress"
    fi
    printf '%s' '{{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"'"$status"'","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{{"number":42}}]}}' ;;
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
    printf '%s' '{"workflow_runs":[{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}]}' ;;
  *"actions/runs?status="*) printf '%s' '{"workflow_runs":[]}' ;;
  "api repos/owner/repo/actions/runs/1")
    printf '%s' '{"id":1,"workflow_id":77,"name":"Required","head_sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","head_branch":"feature","status":"queued","event":"pull_request","created_at":"2026-07-26T00:00:00Z","pull_requests":[{"number":42}]}' ;;
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
