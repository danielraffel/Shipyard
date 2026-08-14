use super::{
    DurableMutationIntent, HOLD_FILE, MergeQueueMutationGuard, hold, hold_status,
    hold_with_lock_boundary_signal, preflight_mutation_authority, resolve_uncertainty, resume,
    uncertain_mutations,
};
use crate::identity::RuntimeMode;
use crate::ship_state::{ShipState, ShipStateStore};

fn fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    ShipStateStore,
    ShipState,
) {
    let temp = tempfile::tempdir().expect("temp");
    let cwd = temp.path().join("repo");
    std::fs::create_dir_all(cwd.join(".shipyard")).expect("config dir");
    std::fs::write(
        cwd.join(".shipyard/config.toml"),
        "[merge_queue]\nmutation_machine = \"studio\"\n",
    )
    .expect("config");
    let state_root = temp.path().join("state");
    std::fs::create_dir_all(&state_root).expect("state root");
    let store = ShipStateStore::new(state_root.join("ship")).expect("store");
    let state = ShipState::new(
        42,
        "owner/repo",
        "feature",
        "main",
        "a".repeat(40),
        "policy",
    );
    (temp, cwd, store, state)
}

fn trusted_global_dir(cwd: &std::path::Path) -> std::path::PathBuf {
    cwd.join(".shipyard")
}

#[test]
fn durable_mutation_intent_round_trips_fresh_and_legacy_correlations() {
    let fresh = DurableMutationIntent::new();
    let second = DurableMutationIntent::new();
    assert_ne!(fresh.correlation_id(), second.correlation_id());
    let resumed = DurableMutationIntent::resume(fresh.correlation_id()).expect("valid correlation");
    assert_eq!(resumed, fresh);
    assert_eq!(
        DurableMutationIntent::resume("legacy-persisted-id")
            .expect("legacy durable correlation")
            .correlation_id(),
        "legacy-persisted-id"
    );
    assert!(DurableMutationIntent::resume("").is_err());
}

fn write_terminal_mutation(state_root: &std::path::Path, correlation_id: &str) {
    std::fs::create_dir_all(state_root.join("merge_queue")).expect("control dir");
    std::fs::write(
        state_root.join("merge_queue/mutations.jsonl"),
        format!(
            "{{\"correlation_id\":\"{correlation_id}\",\"phase\":\"started\"}}\n\
             {{\"correlation_id\":\"{correlation_id}\",\"phase\":\"finished\",\"outcome\":\"superseded\"}}\n"
        ),
    )
    .expect("terminal audit");
}

#[test]
fn terminal_mutation_is_idempotent_while_mutations_are_held() {
    let (_temp, cwd, store, _state) = fixture();
    let state_root = store.path().parent().expect("state root");
    let intent = DurableMutationIntent::resume("already-terminal").expect("intent");
    write_terminal_mutation(state_root, intent.correlation_id());
    hold(state_root, "incident").expect("hold");

    assert!(
        !intent
            .supersede_if_uncertain(
                state_root,
                &trusted_global_dir(&cwd),
                "fresh terminal observation",
            )
            .expect("terminal correlation is an idempotent no-op")
    );
}

#[test]
fn terminal_mutation_is_idempotent_without_local_authority() {
    let (_temp, cwd, store, _state) = fixture();
    let state_root = store.path().parent().expect("state root");
    let intent = DurableMutationIntent::resume("already-terminal").expect("intent");
    write_terminal_mutation(state_root, intent.correlation_id());

    std::fs::write(state_root.join("machine-tag"), "other-machine\n").expect("wrong tag");
    assert!(
        !intent
            .supersede_if_uncertain(
                state_root,
                &trusted_global_dir(&cwd),
                "fresh terminal observation",
            )
            .expect("terminal correlation does not require matching authority")
    );

    std::fs::remove_file(cwd.join(".shipyard/config.toml")).expect("remove policy");
    std::fs::remove_file(state_root.join("machine-tag")).expect("remove tag");
    assert!(
        !intent
            .supersede_if_uncertain(
                state_root,
                &trusted_global_dir(&cwd),
                "fresh terminal observation",
            )
            .expect("terminal correlation does not require configured authority")
    );
}

#[test]
fn uncertain_mutation_still_fails_closed_without_local_authority() {
    let (_temp, cwd, store, _state) = fixture();
    let state_root = store.path().parent().expect("state root");
    let intent = DurableMutationIntent::resume("still-uncertain").expect("intent");
    std::fs::create_dir_all(state_root.join("merge_queue")).expect("control dir");
    std::fs::write(
        state_root.join("merge_queue/mutations.jsonl"),
        format!(
            "{{\"correlation_id\":\"{}\",\"phase\":\"started\"}}\n",
            intent.correlation_id()
        ),
    )
    .expect("uncertain audit");
    std::fs::write(state_root.join("machine-tag"), "other-machine\n").expect("wrong tag");

    let error = intent
        .supersede_if_uncertain(
            state_root,
            &trusted_global_dir(&cwd),
            "fresh terminal observation",
        )
        .expect_err("uncertain correlation requires matching authority");
    assert!(error.contains("authority is `studio`"), "{error}");
    assert!(
        intent
            .is_uncertain(state_root)
            .expect("uncertainty remains"),
        "failed authority must not append a terminal audit record"
    );
}

fn acquire_guard(
    store: &ShipStateStore,
    cwd: &std::path::Path,
    state: &ShipState,
    action: &str,
) -> Result<MergeQueueMutationGuard, String> {
    MergeQueueMutationGuard::acquire_in_mode(
        store,
        cwd,
        RuntimeMode::Shipyard,
        &trusted_global_dir(cwd),
        state,
        action,
    )
}

fn preflight(
    state_root: &std::path::Path,
    cwd: &std::path::Path,
    repo: &str,
    base: &str,
) -> Result<super::MergeQueueMutationPreflight, String> {
    preflight_mutation_authority(
        state_root,
        cwd,
        RuntimeMode::Shipyard,
        &trusted_global_dir(cwd),
        repo,
        base,
    )
}

#[test]
fn authority_machine_and_hold_fail_closed_before_mutation() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "m1\n").expect("tag");
    let error = acquire_guard(&store, &cwd, &state, "enqueue pull request")
        .expect_err("wrong machine rejected");
    assert!(error.contains("authority is `studio`"), "{error}");
    let error = preflight(state_root, &cwd, "owner/repo", "main")
        .expect_err("wrong machine preflight rejected");
    assert!(error.contains("authority is `studio`"), "{error}");

    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    let hold_path = hold(state_root, "incident").expect("hold");
    assert!(hold_path.exists());
    assert_eq!(
        hold_status(state_root).expect("read status").expect("held")["reason"],
        "incident"
    );
    let error =
        acquire_guard(&store, &cwd, &state, "enqueue pull request").expect_err("hold rejected");
    assert!(error.contains("centrally held"));
    let error =
        preflight(state_root, &cwd, "owner/repo", "main").expect_err("hold preflight rejected");
    assert!(error.contains("centrally held"));
    assert!(resume(state_root).expect("resume"));
    assert!(!resume(state_root).expect("idempotent resume"));
}

#[test]
fn malformed_hold_status_fails_closed() {
    let (temp, _, _, _) = fixture();
    let state_root = temp.path().join("state");
    std::fs::create_dir_all(state_root.join("merge_queue")).expect("control dir");
    std::fs::write(state_root.join(HOLD_FILE), "not-json").expect("hold");
    let error = hold_status(&state_root).expect_err("malformed hold rejected");
    assert!(error.contains("mutations remain blocked"));
}

#[test]
fn malformed_authority_configuration_fails_closed() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(
        cwd.join(".shipyard/config.toml"),
        "[merge_queue]\nmutation_machine = 1\n",
    )
    .expect("config");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    let error =
        acquire_guard(&store, &cwd, &state, "enqueue").expect_err("malformed policy rejected");
    assert!(error.contains("must be a non-empty string"));
}

#[test]
fn repository_overlay_cannot_override_trusted_global_authority() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::create_dir_all(cwd.join(".shipyard-dev.local")).expect("isolated config dir");
    std::fs::write(
        cwd.join(".shipyard-dev.local/config.toml"),
        "[merge_queue]\nmutation_machine = \"m1\"\n",
    )
    .expect("isolated config");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");

    let guard = MergeQueueMutationGuard::acquire_in_mode(
        &store,
        &cwd,
        RuntimeMode::Isolated,
        &trusted_global_dir(&cwd),
        &state,
        "enqueue",
    )
    .expect("repository overlay ignored");
    guard.finish("success").expect("finish");
}

#[test]
fn mutation_lock_serializes_and_audits_repo_base_writes() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    let first =
        acquire_guard(&store, &cwd, &state, "enqueue pull request").expect("first authority");
    let error = acquire_guard(&store, &cwd, &state, "dequeue drifted PR")
        .expect_err("second writer rejected");
    assert!(error.contains("another Shipyard process"));
    first.finish("success").expect("finish");

    let second =
        acquire_guard(&store, &cwd, &state, "dequeue drifted PR").expect("authority released");
    second.finish("rejected").expect("finish");

    let audit =
        std::fs::read_to_string(state_root.join("merge_queue/mutations.jsonl")).expect("audit");
    let entries = audit
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("json line"))
        .collect::<Vec<_>>();
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[0]["phase"], "started");
    assert_eq!(entries[1]["outcome"], "success");
    assert_eq!(entries[2]["action"], "dequeue drifted PR");
    assert_eq!(entries[3]["outcome"], "rejected");
    assert_eq!(entries[0]["machine"], "studio");
    assert_eq!(entries[0]["head"], "a".repeat(40));
}

#[test]
fn unmatched_audit_start_is_reported_as_uncertain() {
    let (temp, _, _, _) = fixture();
    let state_root = temp.path().join("state");
    std::fs::create_dir_all(state_root.join("merge_queue")).expect("control dir");
    std::fs::write(
        state_root.join("merge_queue/mutations.jsonl"),
        concat!(
            "{\"correlation_id\":\"done\",\"phase\":\"started\",\"action\":\"enqueue\"}\n",
            "{\"correlation_id\":\"done\",\"phase\":\"finished\",\"outcome\":\"ok\"}\n",
            "{\"correlation_id\":\"orphan\",\"phase\":\"started\",\"action\":\"dequeue\"}\n",
            "{\"correlation_id\":\"ambiguous\",\"phase\":\"started\",\"action\":\"enqueue\"}\n",
            "{\"correlation_id\":\"ambiguous\",\"phase\":\"finished\",\"outcome\":\"uncertain\"}\n",
        ),
    )
    .expect("audit");

    let uncertain = uncertain_mutations(&state_root).expect("audit");
    assert_eq!(uncertain.len(), 2);
    assert_eq!(uncertain[0]["correlation_id"], "ambiguous");
    assert_eq!(uncertain[0]["outcome"], "uncertain");
    assert_eq!(uncertain[1]["correlation_id"], "orphan");
    assert_eq!(uncertain[1]["outcome"], "uncertain");
}

#[test]
fn malformed_or_unreadable_audit_fails_closed() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    std::fs::create_dir_all(state_root.join("merge_queue")).expect("control dir");
    let audit_path = state_root.join("merge_queue/mutations.jsonl");
    std::fs::write(&audit_path, "{\"correlation_id\":\"partial\"").expect("audit");

    let error = acquire_guard(&store, &cwd, &state, "enqueue pull request")
        .expect_err("malformed audit rejected");
    assert!(
        error.contains("malformed merge-queue mutation audit"),
        "{error}"
    );
    assert!(error.contains("mutations remain blocked"), "{error}");

    std::fs::remove_file(&audit_path).expect("remove audit");
    std::fs::create_dir(&audit_path).expect("unreadable audit path");
    let error = acquire_guard(&store, &cwd, &state, "enqueue pull request")
        .expect_err("unreadable audit rejected");
    assert!(
        error.contains("failed to read merge-queue mutation audit"),
        "{error}"
    );
    assert!(error.contains("mutations remain blocked"), "{error}");
}

#[test]
fn preflight_ignores_uncertainty_from_another_repository() {
    let (_temp, cwd, store, _state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    std::fs::create_dir_all(state_root.join("merge_queue")).expect("control dir");
    std::fs::write(
        state_root.join("merge_queue/mutations.jsonl"),
        "{\"correlation_id\":\"other\",\"phase\":\"started\",\"repo\":\"owner/other\",\"base\":\"main\"}\n",
    )
    .expect("audit");
    let preflight = preflight(state_root, &cwd, "owner/repo", "main")
        .expect("unrelated uncertainty does not block");
    drop(preflight);
}

#[test]
fn hold_waits_for_admitted_mutation_and_blocks_later_writers() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root").to_path_buf();
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    let guard =
        acquire_guard(&store, &cwd, &state, "enqueue pull request").expect("mutation admitted");
    let (boundary_tx, boundary_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let hold_root = state_root.clone();
    let thread = std::thread::spawn(move || {
        let result = hold_with_lock_boundary_signal(&hold_root, "incident", || {
            boundary_tx.send(()).expect("lock boundary");
        });
        done_tx.send(result).expect("done");
    });
    boundary_rx
        .recv()
        .expect("hold reached control-lock boundary");
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "hold returned before the admitted mutation released authority"
    );

    guard.finish("success").expect("finish");
    done_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("hold completed")
        .expect("hold succeeded");
    thread.join().expect("hold thread");
    let error = acquire_guard(&store, &cwd, &state, "dequeue pull request")
        .expect_err("later writer blocked");
    assert!(error.contains("centrally held"));
}

#[test]
fn preflight_keeps_control_serialized_through_audited_handoff() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root").to_path_buf();
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    let preflight = preflight(&state_root, &cwd, "owner/repo", "main").expect("preflight");
    let (boundary_tx, boundary_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let hold_root = state_root.clone();
    let thread = std::thread::spawn(move || {
        done_tx
            .send(hold_with_lock_boundary_signal(
                &hold_root,
                "incident",
                || {
                    boundary_tx.send(()).expect("lock boundary");
                },
            ))
            .expect("send hold result");
    });
    boundary_rx
        .recv()
        .expect("hold reached control-lock boundary");
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(50))
            .is_err(),
        "hold crossed the preflight-to-mutation handoff"
    );
    let guard = MergeQueueMutationGuard::acquire_after_preflight(
        preflight,
        &store,
        &cwd,
        RuntimeMode::Shipyard,
        &state,
        "enqueue pull request",
    )
    .expect("audited handoff");
    guard.finish("success").expect("finish");
    done_rx
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("hold completed")
        .expect("hold succeeded");
    thread.join().expect("hold thread");
}

#[test]
fn uncertain_mutation_blocks_retry_until_explicit_resolution() {
    let (_temp, cwd, store, mut state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    state.merge_queue_enqueue_started_at = Some(chrono::Utc::now());
    store.save(&state).expect("state");
    let guard =
        acquire_guard(&store, &cwd, &state, "enqueue pull request").expect("first mutation");
    let correlation_id = guard.correlation_id.clone();
    drop(guard);

    let error = acquire_guard(&store, &cwd, &state, "enqueue pull request")
        .expect_err("uncertain retry blocked");
    assert!(error.contains("is uncertain"));
    resolve_uncertainty(
        state_root,
        &correlation_id,
        "rejected",
        "not present in queue",
    )
    .expect("resolution");
    let resolved = store.get(state.pr).expect("resolved state");
    assert!(resolved.merge_queue_enqueue_started_at.is_none());
    assert!(resolved.merge_queue_enqueue_succeeded_at.is_none());
    let retry = acquire_guard(&store, &cwd, &state, "enqueue pull request")
        .expect("retry admitted after resolution");
    retry.finish("success").expect("finish");
}

#[test]
fn revocation_resolution_allows_live_base_and_preserves_enqueue_proof() {
    let (_temp, cwd, store, mut state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    state.merge_queue_enqueue_succeeded_at = Some(chrono::Utc::now());
    store.save(&state).expect("state");
    let mut live_base_state = state.clone();
    live_base_state.base_branch = "release".to_owned();
    let guard =
        acquire_guard(&store, &cwd, &live_base_state, "dequeue drifted PR").expect("revocation");
    let correlation_id = guard.correlation_id.clone();
    drop(guard);

    resolve_uncertainty(
        state_root,
        &correlation_id,
        "accepted",
        "PR absent from queue",
    )
    .expect("resolution");
    let resolved = store.get(state.pr).expect("state");
    assert_eq!(
        resolved.merge_queue_enqueue_succeeded_at,
        state.merge_queue_enqueue_succeeded_at
    );
}

#[test]
fn identity_drift_skips_state_rewrite_but_allows_audit_resolution() {
    let (_temp, cwd, store, state) = fixture();
    let state_root = store.path().parent().expect("state root");
    std::fs::write(state_root.join("machine-tag"), "studio\n").expect("tag");
    store.save(&state).expect("state");
    let guard = acquire_guard(&store, &cwd, &state, "enqueue pull request").expect("mutation");
    let correlation_id = guard.correlation_id.clone();
    drop(guard);

    let mut advanced = state.clone();
    advanced.head_sha = "b".repeat(40);
    advanced.merge_queue_enqueue_started_at = Some(chrono::Utc::now());
    store.save(&advanced).expect("advanced state");
    resolve_uncertainty(
        state_root,
        &correlation_id,
        "rejected",
        "old head not present in queue",
    )
    .expect("resolution despite identity drift");

    let resolved = store.get(state.pr).expect("state");
    assert_eq!(resolved.head_sha, advanced.head_sha);
    assert_eq!(
        resolved.merge_queue_enqueue_started_at,
        advanced.merge_queue_enqueue_started_at
    );
    assert!(uncertain_mutations(state_root).expect("audit").is_empty());
}
