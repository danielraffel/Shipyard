use super::*;

#[cfg(unix)]
use crate::merge_steward::{RequiredCheck, StewardCheck, StewardCheckSource, StewardPullRequest};

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
#[cfg(unix)]
const GROUP: &str = "cccccccccccccccccccccccccccccccccccccccc";

#[test]
fn apply_without_a_class_is_refused() {
    let error = parse_classes(&[], true).expect_err("refused");
    assert_eq!(error.code, 2);
    assert!(error.message.contains("--class"), "{}", error.message);
    assert!(
        parse_classes(&[], false)
            .expect("plan needs no class")
            .is_empty()
    );
}

#[test]
fn update_branch_can_be_planned_but_never_applied() {
    let classes = parse_classes(&["update_branch".to_owned()], false).expect("plan");
    assert!(classes.contains(&CarrierClass::UpdateBranch));
    let error = parse_classes(&["redispatch".to_owned(), "update-branch".to_owned()], true)
        .expect_err("refused");
    assert!(error.message.contains("own-lines"), "{}", error.message);
}

#[test]
fn unknown_classes_are_refused() {
    let error = parse_classes(&["merge".to_owned()], false).expect_err("refused");
    assert!(error.message.contains("unknown carrier class"));
}

#[test]
fn an_intent_with_another_schema_is_refused() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("intent.json");
    fs::write(&path, r#"{"schema_version":2,"actions":[]}"#).expect("write");
    assert!(read_intent(&path).is_err());
    fs::write(
        &path,
        format!(
            r#"{{"schema_version":1,"actions":[{{"repo":"owner/repo","number":42,"head_sha":"{HEAD}","action":"rearm","head":"{HEAD}"}}]}}"#
        ),
    )
    .expect("write");
    let intent = read_intent(&path).expect("intent");
    assert_eq!(
        intent.actions[0].action,
        CarrierAction::Rearm {
            head: HEAD.to_owned()
        }
    );
}

#[test]
fn replay_plans_recorded_facts_without_github() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("facts.jsonl");
    let facts = serde_json::json!({
        "repo": "owner/repo", "number": 42, "head_sha": HEAD, "draft": false,
        "merge_state": "DIRTY", "queue": {"state": "armed_not_queued"},
        "approved_head": true, "required": [], "runs": [],
        "observed_at": "2026-10-09T10:00:00Z"
    });
    fs::write(&path, format!("{facts}\n\n")).expect("write");
    let mut output = Vec::new();
    let code = replay_command(&path, true, &mut output).expect("replay");
    assert_eq!(code, ExitCode::SUCCESS);
    let value: Value = serde_json::from_slice(&output).expect("json");
    assert_eq!(value["command"], "runner.carrier");
    assert_eq!(value["apply"], false);
    assert_eq!(value["plans"][0]["plan"]["hold"], "conflicting");
}

#[test]
fn a_malformed_replay_line_names_its_line() {
    let temp = tempfile::tempdir().expect("temp");
    let path = temp.path().join("facts.jsonl");
    fs::write(&path, "{}\n").expect("write");
    let error = replay_command(&path, true, &mut Vec::new()).expect_err("refused");
    assert!(error.message.contains("line 1"), "{}", error.message);
}

#[cfg(unix)]
fn fake_gh(temp: &tempfile::TempDir, body: &str) -> GitHubActions {
    let path = temp.path().join("gh");
    let log = temp.path().join("gh.log");
    crate::test_support::write_executable_script(
        &path,
        &format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$*\" >> '{}'\n{body}\n",
            log.display()
        ),
    );
    let config = crate::config::LoadedConfig {
        data: toml::Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: crate::config::LocalOverlaySource::None,
    };
    GitHubActions::from_loaded_config(temp.path(), &config).with_gh_binary_for_tests(path)
}

#[cfg(unix)]
fn gh_calls(temp: &tempfile::TempDir) -> String {
    fs::read_to_string(temp.path().join("gh.log")).unwrap_or_default()
}

#[cfg(unix)]
fn authority(temp: &tempfile::TempDir, machine: &str) -> (ShipStateStore, PathBuf) {
    let global_dir = temp.path().join("global");
    let state_dir = temp.path().join("state");
    fs::create_dir_all(&global_dir).expect("global");
    fs::create_dir_all(&state_dir).expect("state");
    fs::write(
        global_dir.join("config.toml"),
        "[merge_queue]\nmutation_machine = \"m5s\"\n",
    )
    .expect("authority");
    fs::write(state_dir.join("machine-tag"), format!("{machine}\n")).expect("tag");
    (
        ShipStateStore::new(state_dir.join("ship")).expect("store"),
        global_dir,
    )
}

#[cfg(unix)]
fn ejected_observation() -> RepoObservation {
    let checks = vec![StewardCheck {
        name: "macos".to_owned(),
        source: StewardCheckSource::CheckRun,
        app_id: None,
        status: "COMPLETED".to_owned(),
        conclusion: Some("SUCCESS".to_owned()),
        run_id: Some(100),
        observed_at: Some("2026-10-09T09:00:00Z".to_owned()),
    }];
    RepoObservation {
        repo: "owner/repo".to_owned(),
        base: "main".to_owned(),
        allow_auto_merge: true,
        merge_queue: true,
        required_checks: vec![RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        }],
        prs: vec![ObservedPr {
            node_id: "PR_kw".to_owned(),
            fact: StewardPullRequest {
                number: 42,
                head_sha: HEAD.to_owned(),
                head_branch: "feature".to_owned(),
                draft: false,
                merge_state: "BLOCKED".to_owned(),
                auto_merge_active: false,
                queue_position: None,
                labels: Vec::new(),
                checks,
            },
            check_rollup_maybe_truncated: false,
        }],
        runs: Vec::new(),
        merge_group_heads: BTreeMap::new(),
        merge_group_enqueued_at: BTreeMap::new(),
        capacity_preemption_policy: crate::merge_steward::CapacityPreemptionPolicy::for_repository(
            "owner/repo",
        ),
        preemption_error: None,
    }
}

/// A fake `gh` for a pull request the queue removed for `failed_checks` at
/// the current head, whose merge-group `macos` job ended as `macos_job`. The
/// head's first check suite is 07:00; it was armed at 08:00.
#[cfg(unix)]
fn ejected_gh_body(macos_job: &str, arm_reply: &str) -> String {
    ejected_gh_body_armed_at(macos_job, arm_reply, "2026-10-09T08:00:00Z")
}

#[cfg(unix)]
fn ejected_gh_body_armed_at(macos_job: &str, arm_reply: &str, armed_at: &str) -> String {
    format!(
        r#"
case "$*" in
  *enablePullRequestAutoMerge*) printf '%s' '{arm_reply}' ;;
  *timelineItems*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[{{"__typename":"PullRequestCommit","commit":{{"oid":"{HEAD}","checkSuites":{{"nodes":[{{"createdAt":"2026-10-09T07:00:00Z"}}]}}}}}},{{"__typename":"AutoMergeEnabledEvent","createdAt":"{armed_at}","actor":{{"login":"shipyard-local"}}}},{{"__typename":"AddedToMergeQueueEvent","createdAt":"2026-10-09T08:01:00Z","actor":{{"login":"shipyard-local"}}}},{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-10-09T08:30:00Z","reason":"failed_checks","actor":{{"login":"github-merge-queue"}},"beforeCommit":{{"oid":"{GROUP}","parents":{{"nodes":[{{"oid":"dddddddddddddddddddddddddddddddddddddddd"}},{{"oid":"{HEAD}"}}]}}}}}}]}}}}}}}}}}' ;;
  *issues/42/comments*) printf '%s' '[{{"id":77,"body":"Review of {HEAD}: approved.\nreviewed:{HEAD}"}}]' ;;
  *"actions/runs?head_sha={GROUP}&per_page=100&event=merge_group"*) printf '%s' '{{"total_count":1,"workflow_runs":[{{"id":500,"path":".github/workflows/build.yml","name":"Build","event":"merge_group","head_sha":"{GROUP}","status":"completed","conclusion":"failure","run_attempt":1,"run_started_at":"2026-10-09T08:02:00Z"}}]}}' ;;
  *"actions/runs/500/jobs"*) printf '%s' '{{"total_count":2,"jobs":[{{"name":"Linux (x64) [github-hosted]","status":"completed","conclusion":"failure","runner_name":"hosted-1"}},{macos_job}]}}' ;;
  *) printf '%s' '{{}}' ;;
esac
"#
    )
}

#[cfg(unix)]
const STARVED_MACOS: &str = r#"{"name":"macos","status":"completed","conclusion":"cancelled","runner_name":"","created_at":"2026-10-09T08:02:00Z","completed_at":"2026-10-09T08:19:00Z"}"#;
#[cfg(unix)]
const ARMED: &str = r#"{"data":{"enablePullRequestAutoMerge":{"pullRequest":{"number":42}}}}"#;

#[cfg(unix)]
fn planned_report(actions: &GitHubActions, observation: &RepoObservation) -> CarrierRepoReport {
    let facts = carrier_facts(actions, observation, &observation.prs[0]).expect("facts");
    CarrierRepoReport {
        repo: observation.repo.clone(),
        base: observation.base.clone(),
        prs: vec![PlannedPr {
            plan: plan(&facts),
            facts,
            mutation: None,
            error: None,
        }],
        errors: Vec::new(),
    }
}

#[cfg(unix)]
fn rearm_intent() -> CarrierIntent {
    CarrierIntent {
        schema_version: 1,
        actions: vec![IntentAction {
            repo: "owner/repo".to_owned(),
            number: 42,
            head_sha: HEAD.to_owned(),
            action: CarrierAction::Rearm {
                head: HEAD.to_owned(),
            },
        }],
    }
}

#[cfg(unix)]
#[test]
fn a_starved_removal_is_planned_as_an_exact_head_rearm_from_github_facts() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, &ejected_gh_body(STARVED_MACOS, ARMED));
    let observation = ejected_observation();
    let report = planned_report(&actions, &observation);
    let planned = &report.prs[0];
    assert_eq!(
        planned.plan.action(),
        Some(&CarrierAction::Rearm {
            head: HEAD.to_owned()
        })
    );
    assert_eq!(
        planned.facts.approval_evidence.as_deref(),
        Some("arm_event:2026-10-09T08:00:00+00:00")
    );
    assert_eq!(planned.facts.review_marker.as_deref(), Some("comment:77"));
}

#[cfg(unix)]
#[test]
fn a_reviewed_marker_without_an_arm_of_this_head_is_not_an_approval() {
    // Armed at 06:00, before this head's first check suite at 07:00: that arm
    // belonged to an earlier head. Any agent can write the marker.
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        &ejected_gh_body_armed_at(STARVED_MACOS, ARMED, "2026-10-09T06:00:00Z"),
    );
    let report = planned_report(&actions, &ejected_observation());
    let facts = &report.prs[0].facts;
    assert!(!facts.approved_head);
    assert_eq!(facts.review_marker.as_deref(), Some("comment:77"));
    assert!(report.prs[0].plan.action().is_none());
}

#[cfg(unix)]
#[test]
fn a_removal_whose_required_job_ran_and_failed_is_held() {
    let temp = tempfile::tempdir().expect("temp");
    let failed =
        r#"{"name":"macos","status":"completed","conclusion":"failure","runner_name":"m5-gate"}"#;
    let actions = fake_gh(&temp, &ejected_gh_body(failed, ARMED));
    let report = planned_report(&actions, &ejected_observation());
    assert!(report.prs[0].plan.action().is_none());
}

#[cfg(unix)]
#[test]
fn apply_rearms_only_the_intended_action_with_the_exact_head() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, &ejected_gh_body(STARVED_MACOS, ARMED));
    let observation = ejected_observation();
    let mut report = planned_report(&actions, &observation);
    let (store, global_dir) = authority(&temp, "m5s");
    let classes = BTreeSet::from([CarrierClass::Rearm]);
    apply_intent(
        &actions,
        &observation,
        &rearm_intent(),
        &classes,
        &store,
        RuntimeMode::Shipyard,
        &global_dir,
        &mut report,
    );
    assert_eq!(
        report.prs[0].mutation.as_deref(),
        Some(format!("armed {HEAD} with expectedHeadOid").as_str()),
        "{:?}",
        report.prs[0].error
    );
    let calls = gh_calls(&temp);
    assert!(calls.contains(&format!("head={HEAD}")), "{calls}");
    assert!(calls.contains("expectedHeadOid"), "{calls}");
}

#[cfg(unix)]
#[test]
fn apply_does_nothing_outside_the_intent_or_the_enabled_classes() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, &ejected_gh_body(STARVED_MACOS, ARMED));
    let observation = ejected_observation();
    let (store, global_dir) = authority(&temp, "m5s");

    // Not in the intent: nothing happens.
    let mut report = planned_report(&actions, &observation);
    let empty = CarrierIntent {
        schema_version: 1,
        actions: Vec::new(),
    };
    let classes = BTreeSet::from([CarrierClass::Rearm]);
    apply_intent(
        &actions,
        &observation,
        &empty,
        &classes,
        &store,
        RuntimeMode::Shipyard,
        &global_dir,
        &mut report,
    );
    assert_eq!(report.prs[0].mutation, None);

    // In the intent, but the class is not graduated.
    let classes = BTreeSet::from([CarrierClass::Redispatch]);
    apply_intent(
        &actions,
        &observation,
        &rearm_intent(),
        &classes,
        &store,
        RuntimeMode::Shipyard,
        &global_dir,
        &mut report,
    );
    assert_eq!(
        report.prs[0].mutation.as_deref(),
        Some("not applied: class not enabled")
    );
    assert!(
        !gh_calls(&temp).contains("enablePullRequestAutoMerge"),
        "{}",
        gh_calls(&temp)
    );
}

#[cfg(unix)]
#[test]
fn an_intent_naming_an_older_head_is_not_applied() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, &ejected_gh_body(STARVED_MACOS, ARMED));
    let observation = ejected_observation();
    let mut report = planned_report(&actions, &observation);
    let (store, global_dir) = authority(&temp, "m5s");
    let stale = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned();
    let intent = CarrierIntent {
        schema_version: 1,
        actions: vec![IntentAction {
            repo: "owner/repo".to_owned(),
            number: 42,
            head_sha: stale.clone(),
            action: CarrierAction::Rearm { head: stale },
        }],
    };
    apply_intent(
        &actions,
        &observation,
        &intent,
        &BTreeSet::from([CarrierClass::Rearm]),
        &store,
        RuntimeMode::Shipyard,
        &global_dir,
        &mut report,
    );
    assert_eq!(report.prs[0].mutation, None);
    assert!(!gh_calls(&temp).contains("enablePullRequestAutoMerge"));
}

#[cfg(unix)]
#[test]
fn a_host_that_is_not_the_mutation_machine_cannot_apply() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, &ejected_gh_body(STARVED_MACOS, ARMED));
    let observation = ejected_observation();
    let mut report = planned_report(&actions, &observation);
    let (store, global_dir) = authority(&temp, "m3");
    apply_intent(
        &actions,
        &observation,
        &rearm_intent(),
        &BTreeSet::from([CarrierClass::Rearm]),
        &store,
        RuntimeMode::Shipyard,
        &global_dir,
        &mut report,
    );
    assert!(report.prs[0].mutation.is_none());
    assert!(report.prs[0].error.is_some());
    assert!(!gh_calls(&temp).contains("enablePullRequestAutoMerge"));
}
