//! The steward's enqueue runs the queue-arm guard's review rule.
//!
//! The steward's own `head_approval` pre-check is held open (this host reads
//! as opted out of review), so what decides is the bundled guard alone,
//! exactly as `ghapp` runs it.
use super::*;
use crate::app::merge_steward_cmd::pr_mutations::enqueue_pull_request;
use crate::queue_arm_gate::tests_support::with_real_guard;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn steward_gh(temp: &tempfile::TempDir, comments: &str) -> GitHubActions {
    let log = temp.path().join("calls");
    fake_gh(
        temp,
        &format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'reviewer_signers = ["pulp-review"]' ;;
  *"--slurp"*"pulls/42/reviews"*) printf '%s' '[[]]' ;;
  *"--slurp"*"issues/42/comments"*) printf '%s' '[[{comments}]]' ;;
  *"node(id:"*) printf '%s' '{{"data":{{"node":{{"number":42,"state":"OPEN","headRefOid":"{head}","baseRefName":"main","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"repository":{{"nameWithOwner":"Generous-Corp/pulp"}},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}' ;;
  "api repos/Generous-Corp/pulp/pulls/42") printf '%s' '{{"number":42,"base":{{"ref":"main"}},"head":{{"sha":"{head}"}},"user":{{"login":"shipyard-local[bot]","type":"Bot"}}}}' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"chore/codeowners-quiet","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":{{"text":"stacked_pr_mode = \"observe\"\n"}},"pullRequest":{{"headRefOid":"{head}","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) printf '%s' '{{"data":{{"enqueuePullRequest":{{"mergeQueueEntry":{{"position":1}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            head = HEAD,
        ),
    )
}

fn approval(signer: &str) -> String {
    format!(
        r#"{{"id":6390564321,"user":{{"login":"shipyard-local[bot]","type":"Bot"}},"author_association":"CONTRIBUTOR","created_at":"2026-10-24T22:53:36Z","body":"reviewer: {signer}\n<!-- session: Daniels-Mac-Studio-m3/08fb81d8 -->\nverdict: APPROVE head:{HEAD}"}}"#
    )
}

struct Outcome {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
    ledger: Vec<Value>,
}

fn enqueue(comments: &str) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let actions = steward_gh(&temp, comments);
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "Generous-Corp/pulp".to_owned();
    let mut ledger = StewardLedger::default();
    let mutation_control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &mutation_control);
    let arms = temp
        .path()
        .join("state")
        .join(crate::queue_arm_gate::ARMS_LEDGER);
    let guards = temp.path().join("guards");
    fs::create_dir_all(&guards).expect("guards");
    let (mutation, error) = with_real_guard(&arms, &guards, || {
        crate::head_approval::with_host_review_default(false, || {
            enqueue_pull_request(&context, &pr, &queue_policy(), &mut ledger)
        })
    });
    Outcome {
        mutation,
        error,
        calls: fs::read_to_string(temp.path().join("calls")).unwrap_or_default(),
        ledger: fs::read_to_string(&arms)
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).expect("ledger"))
            .collect(),
    }
}

#[test]
fn steward_refuses_to_enqueue_an_unverdicted_head() {
    let outcome = enqueue("");
    assert_eq!(outcome.mutation.as_deref(), Some("waiting_head_approval"));
    let error = outcome.error.expect("refusal");
    assert!(
        error.contains("no reviewer verdict names head aaaaaaaaaaaa"),
        "{error}"
    );
    assert!(
        !outcome.calls.contains("enqueuePullRequest"),
        "{}",
        outcome.calls
    );
    let entry = outcome.ledger.last().expect("ledger line");
    assert_eq!(
        (&entry["verdict"], &entry["pr"], &entry["head"]),
        (&json!("refused"), &json!(42), &json!(HEAD))
    );
}

#[test]
fn steward_refuses_an_approval_from_an_unlisted_signer() {
    let outcome = enqueue(&approval("impostor"));
    assert_eq!(outcome.mutation.as_deref(), Some("waiting_head_approval"));
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("its signer impostor is not one of pulp-review")),
        "{:?}",
        outcome.error
    );
    assert!(!outcome.calls.contains("enqueuePullRequest"));
}

#[test]
fn steward_enqueues_a_signed_exact_head_approval() {
    let outcome = enqueue(&approval("pulp-review"));
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("enqueued"),
        "{:?}",
        outcome.error
    );
    assert!(outcome.calls.contains("enqueuePullRequest"));
    let entry = outcome.ledger.last().expect("ledger line");
    assert_eq!(entry["verdict"], "allowed", "{entry}");
    assert_eq!(entry["verdict_comment_id"], 6_390_564_321_u64, "{entry}");
    assert_eq!(entry["signer"], "pulp-review", "{entry}");
}
