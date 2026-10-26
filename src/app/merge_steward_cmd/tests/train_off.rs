//! The merge train's OFF contract on the steward's managed enqueue: with
//! `[train]` absent or `enabled = false` in the base config, the steward
//! issues byte-identical `gh` calls and reaches the same result.

use super::*;

/// The managed-enqueue fake from `mutations.rs`, with the base config's
/// lines given. `# CONFIG_ERROR` turns off the shared config shim so this
/// script answers the config itself.
fn enqueue_actions(temp: &tempfile::TempDir, log: &Path, config: &str) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
# CONFIG_ERROR
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *contents/.shipyard/config.toml*) printf '%s\n' {config} ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
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
            log = log.display()
        ),
    )
}

/// One managed enqueue of the ready PR: its result and every `gh` argv.
fn enqueue_with_config(config: &str) -> (Option<String>, Option<String>, String) {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = enqueue_actions(&temp, &log, config);
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    let calls = fs::read_to_string(&log).expect("calls");
    (mutation, error, calls)
}

#[test]
fn the_managed_enqueue_is_byte_identical_with_the_train_off() {
    let base = "'[auto_merge]' 'arm_requires_head_approval = false'";
    let baseline = enqueue_with_config(base);
    assert_eq!(baseline.0.as_deref(), Some("enqueued"), "{baseline:?}");
    for off in [
        format!("{base} '[train]'"),
        format!("{base} '[train]' 'enabled = false'"),
    ] {
        assert_eq!(enqueue_with_config(&off), baseline, "{off}");
    }
}
