use super::*;
use crate::app::merge_steward_cmd::recovery::revalidate_recovery_target;

#[test]
fn recovery_revalidation_kills_a_hung_github_read_at_one_absolute_deadline() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"api graphql"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  *"pr view"*) sleep 30 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
    );
    let observed = ready_pr();
    let observation = observation_for(observed.clone(), true);
    let control = mutation_control(&temp, "test-machine", "test-machine");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let policy = queue_policy();
    let deadline = Instant::now() + Duration::from_millis(200);
    let started = Instant::now();

    let (_, error) = revalidate_recovery_target(
        &context,
        &observed,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &StewardLedger::default(),
        deadline,
    )
    .expect_err("hung PR read must fail closed");

    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("timed out")),
        "{error:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(3));
}
