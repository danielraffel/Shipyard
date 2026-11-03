//! A base whose ruleset requires a code-owner review and zero general
//! approvals (Generous-Corp/vellum): the steward waits for the owner's
//! exact-head approval instead of asking GitHub for a certain refusal.
//! vellum #141, 2026-11-02.
use super::*;

const HEAD: &str = "019f774a00000000000000000000000000000000";
const OLD_HEAD: &str = "0000000000000000000000000000000000000001";

/// The vellum ruleset: required checks plus a pull-request rule with zero
/// required approvals and a required code-owner review.
const VELLUM_RULES: &str = r#"[[{"type":"pull_request","parameters":{"required_approving_review_count":0,"require_code_owner_review":true,"dismiss_stale_reviews_on_push":false}}]]"#;

/// A `gh` for one vellum-shaped pull request #42 at [`HEAD`], whose reviews
/// read as `reviews` (`login<TAB>state<TAB>commit` lines) and whose enqueue
/// is answered by `enqueue` (a shell snippet). Every call is logged.
fn vellum_gh(temp: &tempfile::TempDir, log: &Path, reviews: &str, enqueue: &str) -> GitHubActions {
    fake_gh(
        temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"rules/branches/main"*) printf '%s' '{VELLUM_RULES}' ;;
  *"codeowners/errors"*) printf '%s' '{{"errors":[]}}' ;;
  *"contents/.github/CODEOWNERS"*) printf '%s\n' '# vellum' '*  @danielraffel' ;;
  *"pulls/42/files"*) printf '%s\n' 'src/lib.rs' 'docs/guide.md' ;;
  *"pulls/42/reviews"*) printf '{reviews}' ;;
  *"issues/42/comments"*) printf '%s' '{{"id":1}}' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"author":{{"login":"app/shipyard-local","is_bot":true}},"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
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

const ENQUEUED: &str =
    r#"printf '%s' '{"data":{"enqueuePullRequest":{"mergeQueueEntry":{"position":1}}}}'"#;

/// The PR a census row gives: vellum #141's shape, authored by the App.
fn vellum_pr() -> ObservedPr {
    let mut pr = ready_pr();
    pr.fact.head_sha = HEAD.to_owned();
    pr.fact.author = Some("app/shipyard-local".to_owned());
    pr
}

struct Pass {
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
}

fn run_pass(
    temp: &tempfile::TempDir,
    actions: &GitHubActions,
    log: &Path,
    ledger: &mut StewardLedger,
) -> Pass {
    let pr = vellum_pr();
    let observation = observation_for(pr.clone(), true);
    let control = mutation_control(temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(actions, &observation, &ledger_path, &control);
    let (mutation, error) = mutate_pr(
        &context,
        &pr,
        &queue_policy(),
        &StewardDecision::ArmMergeQueue,
        ledger,
    );
    Pass {
        mutation,
        error,
        calls: fs::read_to_string(log).unwrap_or_default(),
    }
}

fn mutation_audit(temp: &tempfile::TempDir) -> Vec<Value> {
    fs::read_to_string(temp.path().join("state/merge_queue/mutations.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).expect("audit line"))
        .collect()
}

#[cfg(unix)]
#[test]
fn the_code_owners_exact_head_approval_admits_the_head() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    // `reviewDecision` would read null here: zero approvals are required.
    let actions = vellum_gh(
        &temp,
        &log,
        &format!(r"danielraffel\tAPPROVED\t{HEAD}\npulp-review[bot]\tAPPROVED\t{HEAD}\n"),
        ENQUEUED,
    );
    let mut ledger = StewardLedger::default();
    let pass = run_pass(&temp, &actions, &log, &mut ledger);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("enqueued"),
        "{:?}",
        pass.error
    );
    assert!(pass.error.is_none(), "{:?}", pass.error);
    assert!(
        pass.calls.contains("contents/.github/CODEOWNERS"),
        "{}",
        pass.calls
    );
    assert_eq!(pass.calls.matches("enqueuePullRequest").count(), 1);
    assert!(!pass.calls.contains("issues/42/comments"), "{}", pass.calls);
}

#[cfg(unix)]
#[test]
fn a_missing_code_owner_approval_waits_without_a_mutation_and_notifies_once() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    // The reviewer approved this head; the code owner has not reviewed it
    // at all. GitHub would refuse the enqueue.
    let actions = vellum_gh(
        &temp,
        &log,
        &format!(r"pulp-review[bot]\tAPPROVED\t{HEAD}\n"),
        ENQUEUED,
    );
    let ledger_path = temp.path().join("ledger.json");
    for pass_number in 0..2 {
        // Each pass starts from the persisted ledger, as each steward tick
        // does.
        let mut ledger = load_ledger(&ledger_path).expect("ledger");
        let pass = run_pass(&temp, &actions, &log, &mut ledger);
        assert_eq!(
            pass.mutation.as_deref(),
            Some("waiting_code_owner:danielraffel"),
            "pass {pass_number}: {:?}",
            pass.error
        );
        // A waiting state, never an error.
        assert!(pass.error.is_none(), "pass {pass_number}: {:?}", pass.error);
        assert!(!pass.calls.contains("enqueuePullRequest"), "{}", pass.calls);
        assert_eq!(
            pass.calls.matches("issues/42/comments").count(),
            1,
            "pass {pass_number}: the owner is told once per head: {}",
            pass.calls
        );
    }
    // No mutation was attempted, so nothing is uncertain or even recorded.
    assert!(
        mutation_audit(&temp).is_empty(),
        "{:?}",
        mutation_audit(&temp)
    );
    let control = mutation_control(&temp, "studio", "studio");
    assert!(
        crate::merge_queue_control::uncertain_mutations(&control.state_dir)
            .expect("uncertain")
            .is_empty()
    );
    // The notice names the owner and the exact head.
    let calls = fs::read_to_string(&log).expect("calls");
    let notice = calls
        .lines()
        .find(|line| line.contains("issues/42/comments"))
        .expect("notice");
    assert!(notice.contains("@danielraffel"), "{notice}");
    assert!(notice.contains(HEAD), "{notice}");
}

#[cfg(unix)]
#[test]
fn an_owners_approval_of_an_earlier_head_is_left_to_github() {
    // vellum keeps stale reviews (`dismiss_stale_reviews_on_push: false`), so
    // GitHub may still count the owner's approval of the previous head.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = vellum_gh(
        &temp,
        &log,
        &format!(r"danielraffel\tAPPROVED\t{OLD_HEAD}\npulp-review[bot]\tAPPROVED\t{HEAD}\n"),
        ENQUEUED,
    );
    let mut ledger = StewardLedger::default();
    let pass = run_pass(&temp, &actions, &log, &mut ledger);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("enqueued"),
        "{:?}",
        pass.error
    );
    assert!(!pass.calls.contains("issues/42/comments"), "{}", pass.calls);
}

#[cfg(unix)]
#[test]
fn a_failed_notice_is_retried_on_the_next_pass() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let marker = temp.path().join("posted-once");
    // The first POST fails; later ones succeed.
    let actions = vellum_gh(
        &temp,
        &log,
        &format!(r"pulp-review[bot]\tAPPROVED\t{HEAD}\n"),
        ENQUEUED,
    );
    let script = temp.path().join("gh");
    let body = fs::read_to_string(&script).expect("script");
    let body = body.replacen(
        r#"*"issues/42/comments"*) printf '%s' '{"id":1}' ;;"#,
        &format!(
            r#"*"issues/42/comments"*) if [ -e '{m}' ]; then printf '%s' '{{"id":1}}'; else : > '{m}'; echo 'gh: Server Error (HTTP 502)' >&2; exit 1; fi ;;"#,
            m = marker.display()
        ),
        1,
    );
    fs::write(&script, body).expect("rewrite script");
    let ledger_path = temp.path().join("ledger.json");
    let mut ledger = load_ledger(&ledger_path).expect("ledger");
    let first = run_pass(&temp, &actions, &log, &mut ledger);
    assert_eq!(
        first.mutation.as_deref(),
        Some("waiting_code_owner:danielraffel,code_owner_notice_failed")
    );
    let mut ledger = load_ledger(&ledger_path).expect("ledger");
    let second = run_pass(&temp, &actions, &log, &mut ledger);
    assert_eq!(
        second.mutation.as_deref(),
        Some("waiting_code_owner:danielraffel")
    );
    assert_eq!(
        second.calls.matches("issues/42/comments").count(),
        2,
        "{}",
        second.calls
    );
    let mut ledger = load_ledger(&ledger_path).expect("ledger");
    let third = run_pass(&temp, &actions, &log, &mut ledger);
    assert_eq!(
        third.calls.matches("issues/42/comments").count(),
        2,
        "{}",
        third.calls
    );
}

#[cfg(unix)]
#[test]
fn a_refusal_with_the_owners_approval_present_is_rejected_not_uncertain() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = vellum_gh(
        &temp,
        &log,
        &format!(r"danielraffel\tAPPROVED\t{HEAD}\n"),
        r#"printf '%s' '{"data":{"enqueuePullRequest":null},"errors":[{"type":"UNPROCESSABLE","path":["enqueuePullRequest"],"message":"Pull request Waiting on code owner review from danielraffel."}]}' >&2; echo 'gh: Pull request Waiting on code owner review from danielraffel.' >&2; exit 1"#,
    );
    let mut ledger = StewardLedger::default();
    let pass = run_pass(&temp, &actions, &log, &mut ledger);
    // The pre-check passed, so GitHub was asked once, and its refusal is
    // settled on the spot.
    assert_eq!(
        pass.calls.matches("enqueuePullRequest").count(),
        1,
        "{}",
        pass.calls
    );
    assert_eq!(
        pass.mutation.as_deref(),
        Some("code_owner_review"),
        "{:?}",
        pass.error
    );
    assert!(pass.error.is_none(), "{:?}", pass.error);
    let control = mutation_control(&temp, "studio", "studio");
    assert!(
        crate::merge_queue_control::uncertain_mutations(&control.state_dir)
            .expect("uncertain")
            .is_empty(),
        "a refusal left an uncertain mutation"
    );
    let finished = mutation_audit(&temp)
        .into_iter()
        .filter(|line| line["phase"] == "finished")
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1, "{finished:?}");
    assert_eq!(finished[0]["outcome"], "rejected");
    assert_eq!(
        finished[0]["reason"],
        "Pull request Waiting on code owner review from danielraffel."
    );
}

#[cfg(unix)]
#[test]
fn an_unreadable_code_owner_check_falls_through_to_github() {
    // Control: the check only ever withholds a certain refusal. When it
    // cannot read CODEOWNERS it asks GitHub, as before.
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"rules/branches/main"*) printf '%s' '{VELLUM_RULES}' ;;
  *"contents/.github/CODEOWNERS"*) echo "HTTP 502" >&2; exit 1 ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*)
    printf '%s' '{{"data":{{"repository":{{"mergeQueue":{{"entries":{{"nodes":[],"pageInfo":{{"hasNextPage":false}}}}}}}}}}}}' ;;
  "pr view "*)
    printf '%s' '{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"CLEAN","autoMergeRequest":null,"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}' ;;
  *"stackConfig"*) printf '%s' '{{"data":{{"repository":{{"stackConfig":null,"pullRequest":{{"headRefOid":"{HEAD}","stack":null,"stackEntry":null}}}}}}}}' ;;
  *"stackEntry"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"stack":null,"stackEntry":null}}}}}}}}' ;;
  *"enqueuePullRequest"*) {ENQUEUED} ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
        ),
    );
    let mut ledger = StewardLedger::default();
    let pass = run_pass(&temp, &actions, &log, &mut ledger);
    assert_eq!(
        pass.mutation.as_deref(),
        Some("enqueued"),
        "{:?}",
        pass.error
    );
    assert!(
        ledger
            .audit
            .iter()
            .any(|entry| entry.action.starts_with("code_owner_check_unread:")),
        "{:?}",
        ledger
            .audit
            .iter()
            .map(|entry| entry.action.as_str())
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_code_owner_wait_is_its_own_blocked_on_token_and_stops_the_caller() {
    let pr = vellum_pr();
    let policy = queue_policy();
    let (stall, _) = super::super::blocked_on::classify(
        &super::super::blocked_on::RowFacts {
            pr: &pr.fact,
            decision: &StewardDecision::ArmMergeQueue,
            mutation: Some("waiting_code_owner:danielraffel"),
            error: None,
            approval: super::super::blocked_on::GateRead::Admits,
            enqueued_at: None,
            ejection: None,
        },
        &policy,
    );
    assert_eq!(stall.blocked_on, "waiting_code_owner:danielraffel");
    let next = super::super::next_action::for_row(
        &super::super::next_action::RowContext {
            repo: "Generous-Corp/vellum",
            base: "main",
            pr: &pr.fact,
            policy: &policy,
            error: None,
        },
        &stall,
    );
    let rendered = serde_json::to_value(&next).expect("next action");
    assert_eq!(rendered["actor"], "owner", "{rendered}");
    assert_eq!(rendered["stop"], true, "{rendered}");
    assert!(
        rendered["action"]
            .as_str()
            .is_some_and(|action| action.contains("danielraffel")),
        "{rendered}"
    );
}
