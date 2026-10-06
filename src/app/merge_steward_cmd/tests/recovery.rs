use super::*;
use crate::app::merge_steward_cmd::recovery::revalidate_recovery_target;

/// A fake `gh` whose merge-queue read answers at once and whose PR read
/// hangs, with every invocation appended to `calls.log`.
const HUNG_PR_READ: &str = r#"
echo "$*" >> "$(dirname "$0")/calls.log"
case "$*" in
  *"api graphql"*)
    printf '%s' '{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false}}}}}}' ;;
  *"pr view"*) sleep 30 ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#;

/// Revalidate a ready PR against `deadline`, returning the error, the wall
/// time and the `gh` invocations.
fn revalidate_until(deadline: Instant) -> (Option<String>, Duration, Vec<String>) {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(&temp, HUNG_PR_READ);
    let observed = ready_pr();
    let observation = observation_for(observed.clone(), true);
    let control = mutation_control(&temp, "test-machine", "test-machine");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let policy = queue_policy();
    let started = Instant::now();
    let (_, error) = revalidate_recovery_target(
        &context,
        &observed,
        &policy,
        &StewardDecision::ArmMergeQueue,
        &StewardLedger::default(),
        deadline,
    )
    .expect_err("a revalidation that cannot finish must fail closed");
    let elapsed = started.elapsed();
    let calls = fs::read_to_string(temp.path().join("calls.log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (error, elapsed, calls)
}

/// The two ways the deadline reports itself: a read killed when it passes,
/// or a read never started because the budget was already spent. Which one
/// a loaded host sees is timing, so neither alone is the property.
fn deadline_held(error: Option<&str>) -> bool {
    error.is_some_and(|message| {
        message.contains("timed out") || message.contains("exceeded its bounded deadline")
    })
}

#[test]
fn recovery_revalidation_kills_a_hung_github_read_at_one_absolute_deadline() {
    let (error, elapsed, calls) = revalidate_until(Instant::now() + Duration::from_secs(1));
    assert!(deadline_held(error.as_deref()), "{error:?}");
    assert!(!calls.is_empty(), "control: the call log records reads");
    assert!(
        elapsed < Duration::from_secs(5),
        "the hung read outlived the deadline: {elapsed:?}"
    );
}

#[test]
fn a_spent_recovery_deadline_fails_closed_before_any_read() {
    // The outcome a loaded host reaches when the merge-queue read uses the
    // whole budget, reached here without depending on load.
    let (error, elapsed, calls) = revalidate_until(Instant::now());
    assert!(
        error
            .as_deref()
            .is_some_and(|message| message.contains("exceeded its bounded deadline")),
        "{error:?}"
    );
    assert!(deadline_held(error.as_deref()));
    assert!(calls.is_empty(), "no read starts: {calls:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
}
