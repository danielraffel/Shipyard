//! Definitive enqueue refusals and the auto-reconciliation of uncertain
//! admissions, end to end through the steward's own mutation path.
use super::*;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A `gh` that answers the reads `mutate_pr` makes before an enqueue, then
/// answers the enqueue with `enqueue` (a shell snippet).
fn enqueue_gh(temp: &tempfile::TempDir, log: &Path, enqueue: &str) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":null,"pullRequest":{{"headRefOid":"{HEAD}","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) {enqueue} ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
        ),
    )
}

fn audit_lines(control: &MutationControl) -> Vec<Value> {
    fs::read_to_string(control.state_dir.join("merge_queue/mutations.jsonl"))
        .expect("audit")
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit line"))
        .collect()
}

#[cfg(unix)]
#[test]
fn a_definitive_graphql_refusal_is_rejected_not_uncertain() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    // vellum #136, 2026-11-01: the wrapper exits 1 with GitHub's typed
    // refusal on stderr, followed by gh's own one-line summary.
    let actions = enqueue_gh(
        &temp,
        &log,
        r#"printf '%s' '{"data":{"enqueuePullRequest":null},"errors":[{"type":"UNPROCESSABLE","path":["enqueuePullRequest"],"message":"Pull request Waiting on code owner review from danielraffel."}]}' >&2; echo 'gh: Pull request Waiting on code owner review from danielraffel.' >&2; exit 1"#,
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);

    for pass in 0..2 {
        let (mutation, error) = mutate_pr(
            &context,
            &pr,
            &queue_policy(),
            &StewardDecision::ArmMergeQueue,
            &mut ledger,
        );
        assert_eq!(
            mutation.as_deref(),
            Some("code_owner_review"),
            "pass {pass}: {error:?}"
        );
        assert!(error.is_none(), "pass {pass}: {error:?}");
        assert!(
            crate::merge_queue_control::uncertain_mutations(&control.state_dir)
                .expect("uncertain")
                .is_empty(),
            "pass {pass}: a definitive refusal left an uncertain mutation"
        );
    }
    // Nothing held the second pass back: it asked GitHub again.
    let calls = fs::read_to_string(&log).expect("calls");
    assert_eq!(calls.matches("enqueuePullRequest").count(), 2, "{calls}");
    let finished = audit_lines(&control)
        .into_iter()
        .filter(|line| line["phase"] == "finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 2, "{finished:?}");
    for line in finished {
        assert_eq!(line["outcome"], "rejected", "{line}");
        assert_eq!(
            line["reason"],
            "Pull request Waiting on code owner review from danielraffel."
        );
    }
}

#[cfg(unix)]
#[test]
fn a_transport_failure_stays_uncertain() {
    // Control for the test above, on the same instrument: no typed answer.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = enqueue_gh(
        &temp,
        &log,
        r#"echo "connection reset after request body" >&2; exit 1"#,
    );
    let pr = ready_pr();
    let observation = observation_for(pr.clone(), true);
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        &mut ledger,
    );
    assert!(mutation.is_none());
    assert!(error.is_some());
    assert_eq!(
        crate::merge_queue_control::uncertain_mutations(&control.state_dir)
            .expect("uncertain")
            .len(),
        1
    );
}

/// Leave an uncertain steward enqueue of `pr` at `head`.
fn leave_uncertain(control: &MutationControl, pr: u64, head: &str) -> String {
    let state = ShipState::new(pr, "owner/repo", "feature", "main", head, "runner-steward");
    let guard = MergeQueueMutationGuard::acquire_in_mode(
        &control.store,
        &control.cwd,
        control.mode,
        &control.global_dir,
        &state,
        "runner steward enqueue pull request",
    )
    .expect("guard");
    let id = guard.correlation_id().to_owned();
    drop(guard);
    id
}

/// A `gh` whose queue-state read for PR `n` prints `answers[n]`.
fn queue_state_gh(temp: &tempfile::TempDir, answers: &[(u64, &str)]) -> GitHubActions {
    let mut arms = String::new();
    for (number, body) in answers {
        use std::fmt::Write as _;
        let _ = writeln!(arms, "  *\"number={number}\"*) printf '%s' '{body}' ;;");
    }
    fake_gh(
        temp,
        &format!("case \"$*\" in\n{arms}  *) echo \"unreadable: $*\" >&2; exit 1 ;;\nesac"),
    )
}

fn pr_state(number: u64, state: &str, head: &str, entry: &str, auto: &str) -> String {
    format!(
        r#"{{"data":{{"repository":{{"pullRequest":{{"number":{number},"state":"{state}","headRefOid":"{head}","isInMergeQueue":{queued},"mergeQueueEntry":{entry},"autoMergeRequest":{auto},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}"#,
        queued = entry != "null",
    )
}

#[cfg(unix)]
#[test]
fn uncertain_admissions_are_settled_both_ways_and_logged() {
    let temp = tempfile::tempdir().expect("temp");
    let control = mutation_control(&temp, "studio", "studio");
    let queued = leave_uncertain(&control, 1, HEAD);
    let merged = leave_uncertain(&control, 2, HEAD);
    let refused = leave_uncertain(&control, 3, HEAD);
    let unreadable = leave_uncertain(&control, 4, HEAD);
    let actions = queue_state_gh(
        &temp,
        &[
            (
                1,
                &pr_state(
                    1,
                    "OPEN",
                    HEAD,
                    r#"{"state":"QUEUED","position":0}"#,
                    "null",
                ),
            ),
            (2, &pr_state(2, "MERGED", HEAD, "null", "null")),
            (3, &pr_state(3, "OPEN", HEAD, "null", "null")),
        ],
    );
    let mut ledger = StewardLedger::default();

    let results = super::super::mutation_outcome::reconcile(
        &actions,
        &control,
        "owner/repo",
        "main",
        &mut ledger,
    )
    .expect("reconcile");

    let outcome = |id: &str| {
        results
            .iter()
            .find(|result| result.correlation_id == id)
            .map_or_else(
                || panic!("{id} missing: {results:?}"),
                |result| result.outcome.as_str(),
            )
    };
    assert_eq!(outcome(&queued), "resolved_accepted");
    assert_eq!(outcome(&merged), "resolved_accepted");
    assert_eq!(outcome(&refused), "resolved_rejected");
    assert_eq!(outcome(&unreadable), "uncertain");

    // Only the unreadable one is still uncertain, and it holds only PR #4.
    let left =
        crate::merge_queue_control::uncertain_mutations(&control.state_dir).expect("uncertain");
    assert_eq!(left.len(), 1, "{left:?}");
    assert_eq!(left[0]["pr"], 4);

    // Each settlement is in the mutations ledger with its evidence.
    let resolutions = audit_lines(&control)
        .into_iter()
        .filter(|line| line["resolver"] == "steward-auto-reconcile")
        .collect::<Vec<_>>();
    assert_eq!(resolutions.len(), 3, "{resolutions:?}");
    for line in &resolutions {
        assert!(
            line["outcome"]
                .as_str()
                .is_some_and(|outcome| outcome.starts_with("resolved_")),
            "{line}"
        );
        assert!(
            line["reason"].as_str().is_some_and(|r| !r.is_empty()),
            "{line}"
        );
        assert_eq!(line["repo"], "owner/repo");
    }
    assert_eq!(
        ledger
            .audit
            .iter()
            .filter(|entry| entry.action.starts_with("uncertain_mutation_resolved_"))
            .count(),
        3
    );
}

#[cfg(unix)]
#[test]
fn an_uncertain_admission_on_another_repository_is_left_alone() {
    let temp = tempfile::tempdir().expect("temp");
    let control = mutation_control(&temp, "studio", "studio");
    leave_uncertain(&control, 1, HEAD);
    let actions = fake_gh(&temp, r#"echo "unexpected: $*" >&2; exit 9"#);
    let mut ledger = StewardLedger::default();
    let results = super::super::mutation_outcome::reconcile(
        &actions,
        &control,
        "owner/other",
        "main",
        &mut ledger,
    )
    .expect("reconcile");
    assert!(results.is_empty(), "{results:?}");
    assert_eq!(
        crate::merge_queue_control::uncertain_mutations(&control.state_dir)
            .expect("uncertain")
            .len(),
        1
    );
}
