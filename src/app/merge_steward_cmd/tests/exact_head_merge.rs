//! Server-enforced exact-head merge on a base with no merge queue but a
//! GitHub-enforced, app-bound required approval context.
use super::*;
use crate::app::merge_steward_cmd::cancellation::apply_pr_plans;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MOVED: &str = "cccccccccccccccccccccccccccccccccccccccc";
const ACTIONS_APP: u64 = 15368;

const POLICY_REQUIRED: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]'"#;
const POLICY_NOT_REQUIRED: &str =
    r"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = false'";

fn required(bound: bool) -> Vec<RequiredCheck> {
    vec![
        RequiredCheck {
            context: "macos".to_owned(),
            app_id: None,
        },
        RequiredCheck {
            context: "review-verdict".to_owned(),
            app_id: bound.then_some(ACTIONS_APP),
        },
    ]
}

fn no_queue_policy(required_checks: Vec<RequiredCheck>) -> StewardPolicy {
    StewardPolicy {
        merge_queue: false,
        native_auto_merge: true,
        required_checks,
        ..queue_policy()
    }
}

/// The PR as GitHub reports it. `review-verdict` comes from the head's
/// check-runs (app-bound), not the rollup.
/// The PR as GitHub reports it; its rollup optionally carries an app-less `review-verdict` commit
/// status in `state`, as the canonical workflow posts it.
fn pr_json_with_status(head: &str, clean: bool, armed: bool, status: Option<&str>) -> String {
    let status = status.map_or_else(String::new, |state| {
        format!(
            r#",{{"__typename":"StatusContext","context":"review-verdict","state":"{state}","createdAt":"2026-10-30T00:02:00Z"}}"#
        )
    });
    let state = if clean { "CLEAN" } else { "BLOCKED" };
    let auto_merge = if armed {
        r#"{"enabledAt":"2026-10-30T00:00:00Z"}"#
    } else {
        "null"
    };
    format!(
        r#"{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{head}","headRefName":"feature","mergeStateStatus":"{state}","autoMergeRequest":{auto_merge},"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}{status}]}}"#
    )
}

/// The census row the steward observed.
fn observed_pr(clean: bool, armed: bool) -> ObservedPr {
    observed_pr_with(clean, armed, true, None)
}

/// A REST check-run row shaped as GitHub returns it for the canonical job.
fn rest_check_run(clean: bool) -> String {
    let (status, conclusion) = if clean {
        ("completed", r#""success""#)
    } else {
        ("in_progress", "null")
    };
    format!(
        r#"{{"id":9001,"name":"review-verdict","head_sha":"{HEAD}","status":"{status}","conclusion":{conclusion},"started_at":"2026-10-30T00:00:30Z","completed_at":"2026-10-30T00:01:00Z","details_url":"https://github.com/owner/repo/actions/runs/101/job/9001","app":{{"id":{ACTIONS_APP},"slug":"github-actions"}}}}"#
    )
}

fn observed_pr_with(clean: bool, armed: bool, check_run: bool, status: Option<&str>) -> ObservedPr {
    let mut pr = parse_pr(
        &serde_json::from_str(&pr_json_with_status(HEAD, clean, armed, status)).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR");
    if check_run {
        pr.fact.checks.push(
            crate::app::merge_steward_cmd::observation::parse_rest_check(
                &serde_json::from_str(&rest_check_run(clean)).expect("check run"),
            )
            .expect("check run"),
        );
    }
    pr
}

fn observation(pr: ObservedPr) -> RepoObservation {
    let mut observation = observation_for(pr, false);
    observation.allow_auto_merge = true;
    observation.required_checks = required(true);
    observation
}

fn comment(body: &str) -> String {
    format!(
        r#"{{"id":12345,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-30T12:00:00Z","body":"{body}","performed_via_github_app_id":99}}"#
    )
}

fn approved() -> String {
    comment(&format!("LGTM\\nreviewed: {HEAD}"))
}

struct Fixture<'a> {
    policy: &'a str,
    comments: String,
    /// Head the live `pr view` reports.
    live_head: &'a str,
    /// Head the gate's queue-state read reports.
    queue_head: &'a str,
    clean: bool,
    armed: bool,
    /// Whether the head has the app-bound `review-verdict` check run.
    check_run: bool,
    /// The app-less `review-verdict` commit status, when one exists.
    status: Option<&'a str>,
}

impl Fixture<'_> {
    fn approved_clean() -> Self {
        Fixture {
            policy: POLICY_REQUIRED,
            comments: approved(),
            live_head: HEAD,
            queue_head: HEAD,
            clean: true,
            armed: false,
            check_run: true,
            status: None,
        }
    }

    fn script(&self, log: &Path) -> String {
        let auto_merge = if self.armed {
            r#"{"enabledAt":"2026-10-30T00:00:00Z"}"#
        } else {
            "null"
        };
        let check_runs = if self.check_run {
            rest_check_run(self.clean)
        } else {
            String::new()
        };
        format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) {policy} ;;
  *"enablePullRequestAutoMerge"*) echo "a server-enforced base must never arm" >&2; exit 92 ;;
  *"mergePullRequest("*) printf '%s' '{{"data":{{"mergePullRequest":{{"pullRequest":{{"number":42,"merged":true}}}}}}}}' ;;
  *"disablePullRequestAutoMerge"*) printf '%s' '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":42}}}}}}}}' ;;
  *"enqueuePullRequest"*) echo "a no-queue base must never enqueue" >&2; exit 91 ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
  *"/check-runs?"*) printf '%s' '{{"check_runs":[{check_runs}]}}' ;;
  *"commits/{HEAD}"*) printf '%s' '2026-10-30T00:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{queue_head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":{auto_merge},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *"query=query("*"mergeQueue"*) printf '%s' '{{"data":{{"repository":{{"mergeQueue":null}}}}}}' ;;
  "pr view "*) printf '%s' '{live_pr}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            policy = self.policy,
            comments = self.comments,
            queue_head = self.queue_head,
            live_pr = pr_json_with_status(self.live_head, self.clean, self.armed, self.status),
        )
    }
}

struct Outcome {
    decision: StewardDecision,
    mutation: Option<String>,
    error: Option<String>,
    calls: String,
    ledger: StewardLedger,
    temp: tempfile::TempDir,
}

/// Classify the census row, then run the PR mutation as the steward would.
fn run(fixture: &Fixture<'_>) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(&temp, &fixture.script(&log));
    let pr = observed_pr_with(
        fixture.clean,
        fixture.armed,
        fixture.check_run,
        fixture.status,
    );
    let observation = observation(pr.clone());
    let policy = no_queue_policy(required(true));
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let (mutation, error) = mutate_pr(&context, &pr, &policy, &decision, &mut ledger);
    let calls = fs::read_to_string(log).unwrap_or_default();
    Outcome {
        decision,
        mutation,
        error,
        calls,
        ledger,
        temp,
    }
}

fn assert_no_write(outcome: &Outcome) {
    for write in [
        "mergePullRequest(",
        "enablePullRequestAutoMerge",
        "enqueuePullRequest",
    ] {
        assert!(
            !outcome.calls.contains(write),
            "{write}: {:?} {:?}\n{}",
            outcome.mutation,
            outcome.error,
            outcome.calls
        );
    }
}

#[test]
fn merge_path_requires_no_queue_and_an_app_bound_required_approval_context() {
    assert_eq!(
        no_queue_policy(required(true)).merge_path(),
        "server_enforced_exact_head_merge"
    );
    let refused = [
        no_queue_policy(Vec::new()),
        no_queue_policy(required(true)[..1].to_vec()),
        // Bound by name only: anyone who can post that status satisfies it.
        no_queue_policy(required(false)),
        StewardPolicy {
            approval_context: String::new(),
            ..no_queue_policy(required(true))
        },
    ];
    for policy in refused {
        assert_eq!(
            policy.merge_path(),
            "direct_merge_refused_server_enforcement_required"
        );
        assert_ne!(
            classify_pr(&observed_pr(true, false).fact, &policy, &BTreeMap::new()),
            StewardDecision::ArmMergeQueue
        );
    }
}

#[test]
fn org_queue_path_is_unchanged() {
    let mut policy = queue_policy();
    policy.required_checks = required(true);
    assert_eq!(policy.merge_path(), "native_queue_exact_head");
    assert!(!policy.server_enforced_exact_head_merge());
    assert_eq!(
        classify_pr(&observed_pr(false, false).fact, &policy, &BTreeMap::new()),
        StewardDecision::WaitingRequired {
            contexts: vec![format!("review-verdict (app_id={ACTIONS_APP})")]
        }
    );
}

#[test]
fn only_a_clean_all_green_head_is_a_merge_candidate() {
    let policy = no_queue_policy(required(true));
    assert!(matches!(
        classify_pr(&observed_pr(false, false).fact, &policy, &BTreeMap::new()),
        StewardDecision::WaitingRequired { .. }
    ));
    let mut failed = observed_pr(true, false);
    for check in &mut failed.fact.checks {
        check.conclusion = Some("FAILURE".to_owned());
        check.run_id = None;
    }
    assert!(matches!(
        classify_pr(&failed.fact, &policy, &BTreeMap::new()),
        StewardDecision::RequiredFailed { .. }
    ));
    assert_eq!(
        classify_pr(&observed_pr(true, false).fact, &policy, &BTreeMap::new()),
        StewardDecision::ArmMergeQueue
    );
}

#[cfg(unix)]
#[test]
fn clean_approved_head_is_merged_by_exact_head() {
    let outcome = run(&Fixture::approved_clean());
    assert_eq!(outcome.decision, StewardDecision::ArmMergeQueue);
    assert!(
        outcome.error.is_none(),
        "{:?}\n{}",
        outcome.error,
        outcome.calls
    );
    assert_eq!(outcome.mutation, Some(format!("merged_exact_head:{HEAD}")));
    let merge = outcome
        .calls
        .lines()
        .find(|line| line.contains("mergePullRequest("))
        .expect("merge call");
    assert!(merge.contains("expectedHeadOid:$head"), "{merge}");
    assert!(merge.contains("mergeMethod:MERGE"), "{merge}");
    assert!(merge.contains(&format!("head={HEAD}")), "{merge}");
    assert_eq!(outcome.calls.matches("mergePullRequest(").count(), 1);
    assert!(!outcome.calls.contains("AutoMerge("), "{}", outcome.calls);
    assert!(
        outcome
            .ledger
            .audit
            .iter()
            .any(|entry| entry.action == "merge_exact_head_server_enforced"
                && entry.subject == format!("pr:42:{HEAD}"))
    );
    let audit = fs::read_to_string(
        outcome
            .temp
            .path()
            .join("state/merge_queue/mutations.jsonl"),
    )
    .expect("mutation audit");
    assert!(audit.contains("merged_exact_head"), "{audit}");
    assert!(audit.contains("\"approval_comment_id\":12345"), "{audit}");
}

#[cfg(unix)]
#[test]
fn pending_head_is_neither_armed_nor_merged() {
    let outcome = run(&Fixture {
        clean: false,
        ..Fixture::approved_clean()
    });
    assert!(matches!(
        outcome.decision,
        StewardDecision::WaitingRequired { .. }
    ));
    assert!(outcome.mutation.is_none(), "{:?}", outcome.mutation);
    assert_no_write(&outcome);
}

#[cfg(unix)]
#[test]
fn not_required_approval_is_refused_on_this_path() {
    let outcome = run(&Fixture {
        policy: POLICY_NOT_REQUIRED,
        comments: String::new(),
        ..Fixture::approved_clean()
    });
    assert_eq!(outcome.mutation.as_deref(), Some("waiting_head_approval"));
    assert!(
        outcome
            .error
            .as_deref()
            .is_some_and(|error| error.contains("not required")),
        "{:?}",
        outcome.error
    );
    assert_no_write(&outcome);
}

#[cfg(unix)]
#[test]
fn unapproved_held_or_earlier_head_approval_is_never_merged() {
    let hold = format!(
        "{}\n{}",
        approved(),
        r#"{"id":12346,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-30T13:00:00Z","body":"verdict: HOLD head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#
    );
    for comments in [
        String::new(),
        hold,
        comment(&format!("LGTM\\nreviewed: {MOVED}")),
    ] {
        let outcome = run(&Fixture {
            comments: comments.clone(),
            ..Fixture::approved_clean()
        });
        assert_eq!(
            outcome.mutation.as_deref(),
            Some("waiting_head_approval"),
            "{comments}: {:?}",
            outcome.error
        );
        assert_no_write(&outcome);
    }
}

#[cfg(unix)]
#[test]
fn moved_head_is_never_merged() {
    // GitHub's live PR moved: the gate is re-evaluated on the new head, which
    // carries no approval.
    let outcome = run(&Fixture {
        live_head: MOVED,
        ..Fixture::approved_clean()
    });
    assert_eq!(outcome.mutation.as_deref(), Some("waiting_head_approval"));
    assert_no_write(&outcome);
    // The head moved between the PR read and the gate's queue-state read.
    let outcome = run(&Fixture {
        queue_head: MOVED,
        ..Fixture::approved_clean()
    });
    assert_eq!(
        outcome.mutation.as_deref(),
        Some("skipped_after_live_revalidation")
    );
    assert_no_write(&outcome);
}

#[cfg(unix)]
#[test]
fn an_armed_request_is_disarmed_before_the_merge() {
    let outcome = run(&Fixture {
        armed: true,
        ..Fixture::approved_clean()
    });
    assert!(outcome.error.is_none(), "{:?}", outcome.error);
    assert_eq!(
        outcome.mutation,
        Some(format!(
            "auto_merge_disabled:{HEAD},merged_exact_head:{HEAD}"
        ))
    );
    let disarm = outcome
        .calls
        .find("disablePullRequestAutoMerge")
        .expect("disarm");
    let merge = outcome.calls.find("mergePullRequest(").expect("merge");
    assert!(disarm < merge, "{}", outcome.calls);
}

/// The sweep runs for an owned PR whatever its decision: here the head is
/// unapproved and still waiting, which is exactly when a leftover arm would
/// merge a head nobody judged.
#[cfg(unix)]
#[test]
fn an_armed_request_on_an_unapproved_waiting_head_is_disarmed() {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let fixture = Fixture {
        comments: String::new(),
        clean: false,
        armed: true,
        ..Fixture::approved_clean()
    };
    let actions = fake_gh(&temp, &fixture.script(&log));
    let pr = observed_pr(false, true);
    assert!(pr.fact.auto_merge_active);
    let observation = observation(pr);
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: true,
        preempt_capacity: true,
        max_preemptions_per_head: 1,
        arm_unqueued: false,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        wake_owner: false,
        unapproved_dequeue: crate::app::merge_steward_cmd::DequeueMode::Report,
        apply: true,
        ledger: None,
    };
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let (reports, _failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &no_queue_policy(required(true)),
        &temp.path().join("ledger.json"),
        &mut ledger,
        Some(&control),
    );
    let calls = fs::read_to_string(log).unwrap_or_default();
    assert!(
        matches!(reports[0].decision, StewardDecision::WaitingRequired { .. }),
        "{:?}",
        reports[0]
    );
    assert!(
        reports[0]
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.contains(&format!("auto_merge_disabled:{HEAD}"))),
        "{:?}\n{calls}",
        reports[0]
    );
    assert!(calls.contains("disablePullRequestAutoMerge"), "{calls}");
    assert!(!calls.contains("mergePullRequest("), "{calls}");
    assert!(!calls.contains("enablePullRequestAutoMerge"), "{calls}");
}

/// The shape both live repositories show: the canonical job's check run from
/// github-actions (app 15368) and an app-less status of the same name. The
/// check run satisfies the bound rule; the status alone never does; and a
/// refusing status refuses even when the job's check run is green, because
/// the job succeeds whenever it posts a verdict.
#[cfg(unix)]
#[test]
fn the_app_bound_check_run_counts_and_an_app_less_status_alone_does_not() {
    let both = run(&Fixture {
        status: Some("SUCCESS"),
        ..Fixture::approved_clean()
    });
    assert_eq!(both.decision, StewardDecision::ArmMergeQueue);
    assert_eq!(both.mutation, Some(format!("merged_exact_head:{HEAD}")));

    let status_only = run(&Fixture {
        check_run: false,
        status: Some("SUCCESS"),
        ..Fixture::approved_clean()
    });
    assert_eq!(
        status_only.decision,
        StewardDecision::WaitingRequired {
            contexts: vec![format!("review-verdict (app_id={ACTIONS_APP})")]
        }
    );
    assert_no_write(&status_only);

    let refused = run(&Fixture {
        status: Some("FAILURE"),
        ..Fixture::approved_clean()
    });
    assert_eq!(
        refused.decision,
        StewardDecision::RequiredFailed {
            contexts: vec!["review-verdict (commit status)".to_owned()]
        }
    );
    assert_no_write(&refused);
}
