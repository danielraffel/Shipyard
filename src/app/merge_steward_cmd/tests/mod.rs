use super::*;

#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    use std::os::unix::fs::PermissionsExt;

    let path = temp.path().join("gh");
    fs::write(&path, format!("#!/bin/sh\nset -eu\n{body}\n")).expect("write fake gh");
    let mut permissions = fs::metadata(&path).expect("fake gh metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("chmod fake gh");
    GitHubActions::new(temp.path()).with_gh_binary_for_tests(path)
}

fn mutation_control(temp: &tempfile::TempDir, authority: &str, machine: &str) -> MutationControl {
    let global_dir = temp.path().join("global");
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&global_dir).expect("global config");
    fs::create_dir_all(&state_dir).expect("state");
    fs::write(
        global_dir.join("config.toml"),
        format!("[merge_queue]\nmutation_machine = \"{authority}\"\n"),
    )
    .expect("authority config");
    fs::write(state_dir.join("machine-tag"), format!("{machine}\n")).expect("machine tag");
    MutationControl {
        store: ShipStateStore::new(state_dir.join("ship")).expect("ship store"),
        cwd: temp.path().to_path_buf(),
        mode: RuntimeMode::Shipyard,
        global_dir,
    }
}

fn mutation_apply_context<'a>(
    actions: &'a GitHubActions,
    observation: &'a RepoObservation,
    ledger_path: &'a Path,
    mutation_control: &'a MutationControl,
) -> MutationApplyContext<'a> {
    MutationApplyContext {
        actions,
        observation,
        ledger_path,
        mutation_control,
    }
}

fn ready_pr() -> ObservedPr {
    parse_pr(
        &serde_json::json!({
            "id": "PR_kw",
            "number": 42,
            "state": "OPEN",
            "isDraft": false,
            "headRefOid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "headRefName": "feature",
            "mergeStateStatus": "CLEAN",
            "autoMergeRequest": null,
            "labels": [],
            "statusCheckRollup": [{
                "__typename": "CheckRun",
                "name": "macos",
                "status": "COMPLETED",
                "conclusion": "SUCCESS",
                "detailsUrl": "https://github.com/owner/repo/actions/runs/100"
            }]
        }),
        &BTreeMap::new(),
    )
    .expect("ready PR")
}

fn observation_for(pr: ObservedPr, merge_queue: bool) -> RepoObservation {
    RepoObservation {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: merge_queue,
        merge_queue,
        merge_method: Some("merge".to_owned()),
        required_contexts: Vec::new(),
        prs: vec![pr],
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: CapacityPreemptionPolicy::pulp(),
        preemption_error: None,
    }
}

fn queue_policy() -> StewardPolicy {
    StewardPolicy {
        merge_queue: true,
        native_auto_merge: true,
        required_contexts: Vec::new(),
        opt_out_label: "steward:skip".to_owned(),
        max_transient_reruns: 1,
    }
}

fn queued_run(id: u64, created_at: &str) -> StewardRun {
    StewardRun {
        id,
        workflow_id: 77,
        run_attempt: 1,
        workflow: "Required".to_owned(),
        head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        head_branch: "feature".to_owned(),
        status: "queued".to_owned(),
        event: "pull_request".to_owned(),
        pull_request_number: Some(42),
        created_at: created_at.to_owned(),
        jobs: Vec::new(),
    }
}

fn pending_cancellation_record() -> PendingCancellation {
    PendingCancellation {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        run_id: 100,
        workflow_id: 77,
        run_attempt: 1,
        head_sha: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
        head_branch: "feature".to_owned(),
        pr_number: 42,
        front_head: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
        initiated_at: "2026-07-26T00:00:00Z".to_owned(),
        phase: PendingCancellationPhase::Accepted,
        mutation_correlation_id: "finished-test-mutation".to_owned(),
        mutation_kind: PendingMutationKind::NormalCancel,
        reason: "advisory_preamble_capacity_theft".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
    }
}

mod mutations;
mod observation;
