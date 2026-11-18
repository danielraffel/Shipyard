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
    observed_pr_with(clean, armed, true, None, ACTIONS_BOT)
}

/// The creator GitHub records on a status posted with a workflow's token.
const ACTIONS_BOT: (&str, u64) = ("github-actions[bot]", 41_898_282);

/// A REST commit-status row, which, unlike the rollup, names its creator.
fn rest_status(state: &str, creator: (&str, u64)) -> String {
    let (login, id) = creator;
    format!(
        r#"{{"context":"review-verdict","state":"{}","created_at":"2026-10-30T00:02:00Z","updated_at":"2026-10-30T00:02:00Z","target_url":"https://github.com/owner/repo/actions/runs/101","creator":{{"login":"{login}","id":{id},"type":"Bot"}}}}"#,
        state.to_ascii_lowercase()
    )
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

fn observed_pr_with(
    clean: bool,
    armed: bool,
    check_run: bool,
    status: Option<&str>,
    creator: (&str, u64),
) -> ObservedPr {
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
    // The census reads the status's creator over REST, as hydration does when
    // a bound rule's context appears as a rollup status.
    if let Some(state) = status {
        pr.fact.checks.extend(
            crate::app::merge_steward_cmd::observation::parse_rest_status(
                &serde_json::from_str(&rest_status(state, creator)).expect("status"),
            )
            .filter(|status| status.app_id.is_some()),
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
    /// The `review-verdict` commit status, when one exists.
    status: Option<&'a str>,
    /// Who created that status.
    status_creator: (&'a str, u64),
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
            status_creator: ACTIONS_BOT,
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
        let statuses = self
            .status
            .map_or_else(String::new, |state| rest_status(state, self.status_creator));
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
  *"/statuses?"*) printf '%s' '[{statuses}]' ;;
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
        fixture.status_creator,
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
        infra_rerun: crate::app::merge_steward_cmd::InfraRerunMode::Off,
        pass_budget: None,
        pass_deadline: None,
        invariants: None,
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

/// The shapes the live repositories show: the canonical job's check run from
/// github-actions (app 15368), and a status of the same name that the job
/// posts with its `GITHUB_TOKEN`, so its creator is `github-actions[bot]`.
/// Either satisfies the bound rule, as GitHub's branch protection counts it;
/// a status of that name from any other creator never does; and a refusing
/// status refuses even when the job's check run is green, because the job
/// succeeds whenever it posts a verdict.
#[cfg(unix)]
#[test]
fn an_app_bound_rule_counts_that_apps_check_run_or_status_and_no_one_elses_status() {
    let both = run(&Fixture {
        status: Some("SUCCESS"),
        ..Fixture::approved_clean()
    });
    assert_eq!(both.decision, StewardDecision::ArmMergeQueue);
    assert_eq!(both.mutation, Some(format!("merged_exact_head:{HEAD}")));

    // The job renamed away from the context: only the Actions status remains.
    let status_only = run(&Fixture {
        check_run: false,
        status: Some("SUCCESS"),
        ..Fixture::approved_clean()
    });
    assert_eq!(status_only.decision, StewardDecision::ArmMergeQueue);
    assert_eq!(
        status_only.mutation,
        Some(format!("merged_exact_head:{HEAD}")),
        "{:?}\n{}",
        status_only.error,
        status_only.calls
    );

    for creator in [
        ("octocat", 583_231),
        ("someone-elses-app[bot]", 99_999),
        // The Actions login without its user ID is not the Actions bot.
        ("github-actions[bot]", 1),
    ] {
        let spoofed = run(&Fixture {
            check_run: false,
            status: Some("SUCCESS"),
            status_creator: creator,
            ..Fixture::approved_clean()
        });
        assert_eq!(
            spoofed.decision,
            StewardDecision::WaitingRequired {
                contexts: vec![format!("review-verdict (app_id={ACTIONS_APP})")]
            },
            "{creator:?}"
        );
        assert_no_write(&spoofed);
    }

    let refused = run(&Fixture {
        status: Some("FAILURE"),
        ..Fixture::approved_clean()
    });
    assert!(
        matches!(&refused.decision, StewardDecision::RequiredFailed { contexts }
            if contexts.iter().all(|context| context.starts_with("review-verdict "))),
        "{:?}",
        refused.decision
    );
    assert_no_write(&refused);

    let pending = run(&Fixture {
        check_run: false,
        status: Some("PENDING"),
        ..Fixture::approved_clean()
    });
    assert_eq!(
        pending.decision,
        StewardDecision::WaitingRequired {
            contexts: vec![format!("review-verdict (app_id={ACTIONS_APP})")]
        }
    );
    assert_no_write(&pending);
}

/// The PR as GitHub reports it while the steward's own failed recovery status
/// and its `needs-agent` label are still on the head: everything required is
/// green, but that red non-required status holds the merge state at UNSTABLE.
fn recovery_flagged_pr_json() -> String {
    pr_json_with_status(HEAD, true, false, None)
        .replace(r#""mergeStateStatus":"CLEAN""#, r#""mergeStateStatus":"UNSTABLE""#)
        .replace(r#""labels":[]"#, r#""labels":[{"name":"shipyard:needs-agent"}]"#)
        .replace(
            "]}",
            &format!(
                r#",{{"__typename":"StatusContext","context":"{RECOVERY_CONTEXT}","state":"FAILURE","createdAt":"2026-10-30T00:03:00Z"}}]}}"#
            ),
        )
}

/// The same PR once the clear has landed: label gone, recovery status green,
/// merge state CLEAN.
fn recovery_cleared_pr_json() -> String {
    pr_json_with_status(HEAD, true, false, None).replace(
        "]}",
        &format!(
            r#",{{"__typename":"StatusContext","context":"{RECOVERY_CONTEXT}","state":"SUCCESS","createdAt":"2026-10-30T00:04:00Z"}}]}}"#
        ),
    )
}

/// Run one full steward pass over a recovery-flagged, otherwise-ready head.
/// The fake GitHub reports the cleared shape only after the steward has
/// written its success recovery status, and reports `cleared_merge_state`
/// as the merge state from then on.
fn run_recovery_clear_pass(cleared_merge_state: &str) -> (PrReport, String) {
    run_recovery_clear_pass_under(cleared_merge_state, &no_queue_policy(required(true)), &[])
}

/// Append completed check runs no rule requires to a PR JSON rollup.
fn with_extra_checks(json: &str, extra: &[(&str, &str)]) -> String {
    use std::fmt::Write as _;
    let rows = extra
        .iter()
        .enumerate()
        .fold(String::new(), |mut rows, (index, (name, conclusion))| {
            let _ = write!(
                rows,
                r#",{{"__typename":"CheckRun","name":"{name}","status":"COMPLETED","conclusion":"{conclusion}","completedAt":"2026-10-30T00:00:30Z","detailsUrl":"https://github.com/owner/repo/actions/runs/{}"}}"#,
                400 + index
            );
            rows
        });
    let end = json.rfind("]}").expect("rollup end");
    format!("{}{rows}{}", &json[..end], &json[end..])
}

fn run_recovery_clear_pass_under(
    cleared_merge_state: &str,
    policy: &StewardPolicy,
    extra: &[(&str, &str)],
) -> (PrReport, String) {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let marker = temp.path().join("recovery-cleared");
    let base = Fixture::approved_clean().script(&log);
    let live_line = format!(
        r#"  "pr view "*) printf '%s' '{}' ;;"#,
        pr_json_with_status(HEAD, true, false, None)
    );
    assert!(base.contains(&live_line), "fixture shape changed");
    let cleared = with_extra_checks(
        &recovery_cleared_pr_json().replace("\"CLEAN\"", &format!("\"{cleared_merge_state}\"")),
        extra,
    );
    let flagged = with_extra_checks(&recovery_flagged_pr_json(), extra);
    let script = base.replace(
        &live_line,
        &format!(
            r#"  *"-X POST repos/owner/repo/statuses/{HEAD}"*) : > '{marker}'; printf '%s' '{{}}' ;;
  *"-X DELETE repos/owner/repo/issues/42/labels/"*) printf '%s' '{{}}' ;;
  "api repos/owner/repo/pulls/42") printf '%s' '{{"state":"open","head":{{"sha":"{HEAD}"}}}}' ;;
  "pr view "*) if [ -e '{marker}' ]; then printf '%s' '{cleared}'; else printf '%s' '{flagged}'; fi ;;"#,
            marker = marker.display(),
        ),
    );
    let actions = fake_gh(&temp, &script);
    let mut pr = parse_pr(
        &serde_json::from_str(&flagged).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR");
    pr.fact.checks.push(
        crate::app::merge_steward_cmd::observation::parse_rest_check(
            &serde_json::from_str(&rest_check_run(true)).expect("check run"),
        )
        .expect("check run"),
    );
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
        infra_rerun: crate::app::merge_steward_cmd::InfraRerunMode::Off,
        pass_budget: None,
        pass_deadline: None,
        invariants: None,
        apply: true,
        ledger: None,
    };
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let (mut reports, _failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        policy,
        &temp.path().join("ledger.json"),
        &mut ledger,
        Some(&control),
    );
    let calls = fs::read_to_string(log).unwrap_or_default();
    (reports.remove(0), calls)
}

/// A pass that clears the steward's own stale recovery signal from an
/// otherwise-ready head merges it in that same pass, instead of leaving it to
/// wait on the UNSTABLE merge state the cleared status caused.
#[cfg(unix)]
#[test]
fn clearing_a_stale_recovery_signal_merges_the_ready_head_in_the_same_pass() {
    let (report, calls) = run_recovery_clear_pass("CLEAN");
    assert!(report.error.is_none(), "{report:?}\n{calls}");
    assert_eq!(report.decision, StewardDecision::ArmMergeQueue, "{calls}");
    assert_eq!(
        report.mutation,
        Some(format!("needs_agent_cleared,merged_exact_head:{HEAD}")),
        "{calls}"
    );
    // The clear is written before the merge, and the merge is exact-head.
    let clear = calls.find("state=success").expect("recovery clear");
    let merge = calls.find("mergePullRequest(").expect("merge");
    assert!(clear < merge, "{calls}");
    assert!(calls.contains(&format!("head={HEAD}")), "{calls}");
    assert_eq!(calls.matches("mergePullRequest(").count(), 1, "{calls}");
}

/// When GitHub still reports UNSTABLE after the clear and the base's
/// `unstable_advisory_checks` list is unknown, the pass merges nothing: the
/// re-read is a live classification, not a waiver of the CLEAN requirement.
#[cfg(unix)]
#[test]
fn a_head_still_unstable_after_the_clear_waits_for_the_next_pass() {
    let (report, calls) = run_recovery_clear_pass("UNSTABLE");
    assert!(report.error.is_none(), "{report:?}\n{calls}");
    assert_eq!(
        report.decision,
        StewardDecision::WaitingRequired {
            contexts: vec!["github-merge-state:CLEAN (current=UNSTABLE)".to_owned()]
        }
    );
    assert_eq!(report.mutation.as_deref(), Some("needs_agent_cleared"));
    assert_no_write(&Outcome {
        decision: report.decision.clone(),
        mutation: report.mutation.clone(),
        error: None,
        calls: calls.clone(),
        ledger: StewardLedger::default(),
        temp: tempfile::tempdir().expect("temp"),
    });
}

/// A no-queue policy as the steward reads it from a base config: `listed` is
/// `[auto_merge] unstable_advisory_checks`, `None` when unknown.
fn unstable_policy(listed: Option<&[&str]>) -> StewardPolicy {
    let mut policy = no_queue_policy(required(true));
    policy.ejection_guards.unstable_advisory_checks =
        listed.map(|names| names.iter().map(|name| (*name).to_owned()).collect());
    policy
}

/// The approved head as GitHub reports it `UNSTABLE`, with the required
/// `macos` check concluding `macos` and `extra` rollup rows (checks no rule
/// requires) appended. A name starting `status:` is a commit status with that
/// context and state; any other is a completed check run with that
/// conclusion, or a queued one when the conclusion is `QUEUED`.
fn unstable_pr_json(macos: &str, extra: &[(&str, &str)]) -> String {
    use std::fmt::Write as _;
    let rows = extra
        .iter()
        .enumerate()
        .fold(String::new(), |mut rows, (index, (name, outcome))| {
            if let Some(context) = name.strip_prefix("status:") {
                let _ = write!(
                    rows,
                    r#",{{"__typename":"StatusContext","context":"{context}","state":"{outcome}","createdAt":"2026-10-30T00:00:4{index}Z"}}"#
                );
            } else if *outcome == "QUEUED" {
                let _ = write!(
                    rows,
                    r#",{{"__typename":"CheckRun","name":"{name}","status":"QUEUED","conclusion":null,"completedAt":null,"detailsUrl":"https://github.com/owner/repo/actions/runs/{}"}}"#,
                    300 + index
                );
            } else {
                let _ = write!(
                    rows,
                    r#",{{"__typename":"CheckRun","name":"{name}","status":"COMPLETED","conclusion":"{outcome}","completedAt":"2026-10-30T00:00:30Z","detailsUrl":"https://github.com/owner/repo/actions/runs/{}"}}"#,
                    300 + index
                );
            }
            rows
        });
    let macos_row = if macos == "IN_PROGRESS" {
        r#""status":"IN_PROGRESS","conclusion":null,"completedAt":null"#.to_owned()
    } else {
        format!(
            r#""status":"COMPLETED","conclusion":"{macos}","completedAt":"2026-10-30T00:00:00Z""#
        )
    };
    let json = pr_json_with_status(HEAD, true, false, None)
        .replace(
            r#""mergeStateStatus":"CLEAN""#,
            r#""mergeStateStatus":"UNSTABLE""#,
        )
        .replace(
            r#""status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:00:00Z""#,
            &macos_row,
        )
        .replace("]}", &format!("{rows}]}}"));
    if macos == "MISSING" {
        let start = json
            .find(r#"{"__typename":"CheckRun","name":"macos""#)
            .expect("macos row");
        let end = start + json[start..].find('}').expect("row end") + 1;
        let tail = &json[end..];
        let tail = tail.strip_prefix(',').unwrap_or(tail);
        return format!("{}{tail}", &json[..start]);
    }
    json
}

/// One steward pass (classify the census row, then mutate) over a head the
/// census and the final live read both report as `census` and `live`.
fn run_unstable(policy: &StewardPolicy, census: &str, live: &str) -> Outcome {
    run_unstable_reads(policy, census, live, live)
}

/// [`run_unstable`], with the last live read (the final revalidation under
/// the merge guard) reporting `final_live` while every earlier one reports
/// `live`.
fn run_unstable_reads(
    policy: &StewardPolicy,
    census: &str,
    live: &str,
    final_live: &str,
) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let reads = temp.path().join("pr-view-reads");
    let base = Fixture::approved_clean().script(&log);
    let live_line = format!(
        r#"  "pr view "*) printf '%s' '{}' ;;"#,
        pr_json_with_status(HEAD, true, false, None)
    );
    assert!(base.contains(&live_line), "fixture shape changed");
    // The final revalidation is the pass's second live `pr view`; the first is
    // the pre-guard live re-read.
    let script = base.replace(
        &live_line,
        &format!(
            r#"  "pr view "*) printf x >> '{reads}'; if [ "$(wc -c < '{reads}' | tr -d ' ')" -ge 2 ]; then printf '%s' '{final_live}'; else printf '%s' '{live}'; fi ;;"#,
            reads = reads.display()
        ),
    );
    let actions = fake_gh(&temp, &script);
    let mut pr = parse_pr(
        &serde_json::from_str(census).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR");
    pr.fact.checks.push(
        crate::app::merge_steward_cmd::observation::parse_rest_check(
            &serde_json::from_str(&rest_check_run(true)).expect("check run"),
        )
        .expect("check run"),
    );
    let observation = observation(pr.clone());
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let decision = classify_pr(&pr.fact, policy, &BTreeMap::new());
    let (mutation, error) = mutate_pr(&context, &pr, policy, &decision, &mut ledger);
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

fn assert_merged(outcome: &Outcome) {
    assert_eq!(
        outcome.decision,
        StewardDecision::ArmMergeQueue,
        "{}",
        outcome.calls
    );
    assert!(
        outcome.error.is_none(),
        "{:?}\n{}",
        outcome.error,
        outcome.calls
    );
    assert_eq!(outcome.calls.matches("mergePullRequest(").count(), 1);
    assert!(
        outcome.calls.contains(&format!("head={HEAD}")),
        "{}",
        outcome.calls
    );
}

/// GitHub reports UNSTABLE because only a check the base lists advisory is
/// red; every required check is green and the head is approved, so the
/// steward merges it by exact head and names the advisory check.
#[cfg(unix)]
#[test]
fn unstable_head_red_only_in_an_advisory_check_is_merged() {
    let json = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "FAILURE")]);
    let outcome = run_unstable(
        &unstable_policy(Some(&["Zizmor (report only)"])),
        &json,
        &json,
    );
    assert_merged(&outcome);
    assert_eq!(
        outcome.mutation,
        Some(format!(
            "merged_exact_head:{HEAD},advisory_not_green:zizmor_(report_only)"
        ))
    );
}

/// A red or pending check the base does not list holds an UNSTABLE head,
/// even beside a listed one; the list also has to be known at all.
#[cfg(unix)]
#[test]
fn unstable_head_with_an_unlisted_red_or_pending_check_is_not_merged() {
    let policy = unstable_policy(Some(&["zizmor (report only)"]));
    // Control: only the listed check is red.
    let listed_only = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "FAILURE")]);
    assert_merged(&run_unstable(&policy, &listed_only, &listed_only));

    for outcome in [
        "FAILURE",
        "QUEUED",
        "CANCELLED",
        "TIMED_OUT",
        "ACTION_REQUIRED",
    ] {
        let json = unstable_pr_json(
            "SUCCESS",
            &[("zizmor (report only)", "FAILURE"), ("risk-tier", outcome)],
        );
        let held = run_unstable(&policy, &json, &json);
        assert_eq!(
            held.decision,
            StewardDecision::WaitingRequired {
                contexts: vec![
                    UNSTABLE_WAIT.to_owned(),
                    "risk-tier (not in unstable_advisory_checks)".to_owned(),
                ]
            },
            "{outcome}"
        );
        assert_no_write(&held);
    }

    // An unknown list holds even a head red only in that check.
    let held = run_unstable(&unstable_policy(None), &listed_only, &listed_only);
    assert_eq!(
        held.decision,
        StewardDecision::WaitingRequired {
            contexts: vec![UNSTABLE_WAIT.to_owned()]
        }
    );
    assert_no_write(&held);
}

/// An advisory check reported as a commit status admits like a check run;
/// an unlisted red commit status holds.
#[cfg(unix)]
#[test]
fn an_advisory_commit_status_admits_and_an_unlisted_one_holds() {
    let policy = unstable_policy(Some(&["ci/report"]));
    let listed = unstable_pr_json("SUCCESS", &[("status:ci/report", "FAILURE")]);
    let merged = run_unstable(&policy, &listed, &listed);
    assert_merged(&merged);
    assert_eq!(
        merged.mutation,
        Some(format!(
            "merged_exact_head:{HEAD},advisory_not_green:ci/report"
        ))
    );
    let unlisted = unstable_pr_json(
        "SUCCESS",
        &[
            ("status:ci/report", "FAILURE"),
            ("status:ci/other", "PENDING"),
        ],
    );
    let held = run_unstable(&policy, &unlisted, &unlisted);
    assert_eq!(
        held.decision,
        StewardDecision::WaitingRequired {
            contexts: vec![
                UNSTABLE_WAIT.to_owned(),
                "ci/other (not in unstable_advisory_checks)".to_owned(),
            ]
        }
    );
    assert_no_write(&held);
}

/// A required check that is red, still running, or missing never lands on an
/// UNSTABLE head, whatever the advisory list says, whether the census or only
/// the final live read sees it.
#[cfg(unix)]
#[test]
fn unstable_head_with_a_required_check_not_green_is_never_merged() {
    let policy = unstable_policy(Some(&["zizmor (report only)", "macos"]));
    let green = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "FAILURE")]);
    // Control.
    assert_merged(&run_unstable(&policy, &green, &green));

    for macos in ["FAILURE", "IN_PROGRESS", "MISSING"] {
        let json = unstable_pr_json(macos, &[("zizmor (report only)", "FAILURE")]);
        let census = run_unstable(&policy, &json, &json);
        assert!(
            matches!(
                &census.decision,
                StewardDecision::RequiredFailed { contexts } | StewardDecision::WaitingRequired { contexts }
                    if contexts == &vec!["macos".to_owned()]
            ),
            "{macos}: {:?}",
            census.decision
        );
        assert_no_write(&census);

        let live_only = run_unstable_reads(&policy, &green, &green, &json);
        assert_eq!(
            live_only.decision,
            StewardDecision::ArmMergeQueue,
            "{macos}"
        );
        assert_no_write(&live_only);
    }
}

/// A refusing `review-verdict` commit status holds an UNSTABLE head even
/// with the approval context listed advisory.
#[cfg(unix)]
#[test]
fn a_refusing_review_verdict_status_holds_an_unstable_head() {
    let policy = unstable_policy(Some(&["review-verdict", "zizmor (report only)"]));
    let refused = unstable_pr_json(
        "SUCCESS",
        &[
            ("zizmor (report only)", "FAILURE"),
            ("status:review-verdict", "FAILURE"),
        ],
    );
    let held = run_unstable(&policy, &refused, &refused);
    assert_ne!(held.decision, StewardDecision::ArmMergeQueue);
    assert_no_write(&held);
    // Control: the same head with the status green merges.
    let agreed = unstable_pr_json(
        "SUCCESS",
        &[
            ("zizmor (report only)", "FAILURE"),
            ("status:review-verdict", "SUCCESS"),
        ],
    );
    assert_merged(&run_unstable(&policy, &agreed, &agreed));
}

/// The steward's own recovery status can never be advisory: red at the
/// census, or turning red only at the final live read, it holds the head.
#[cfg(unix)]
#[test]
fn the_stewards_own_red_recovery_status_holds_an_unstable_head() {
    let policy = unstable_policy(Some(&[RECOVERY_CONTEXT, "zizmor (report only)"]));
    let green = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "FAILURE")]);
    let flagged = unstable_pr_json(
        "SUCCESS",
        &[
            ("zizmor (report only)", "FAILURE"),
            (&format!("status:{RECOVERY_CONTEXT}"), "FAILURE"),
        ],
    );
    // Control.
    assert_merged(&run_unstable(&policy, &green, &green));
    let census = run_unstable(&policy, &flagged, &flagged);
    assert_eq!(
        census.decision,
        StewardDecision::WaitingRequired {
            contexts: vec![
                UNSTABLE_WAIT.to_owned(),
                format!("{RECOVERY_CONTEXT} (not in unstable_advisory_checks)"),
            ]
        }
    );
    assert!(crate::merge_steward::waits_only_on_recovery_status(
        match &census.decision {
            StewardDecision::WaitingRequired { contexts } => contexts,
            other => panic!("{other:?}"),
        }
    ));
    assert_no_write(&census);
    let final_only = run_unstable_reads(&policy, &green, &green, &flagged);
    assert_eq!(final_only.decision, StewardDecision::ArmMergeQueue);
    assert_no_write(&final_only);
}

/// GitHub says UNSTABLE but every check the steward sees is green: nothing
/// explains the state, so it holds rather than guess.
#[cfg(unix)]
#[test]
fn an_unexplained_unstable_state_holds() {
    let json = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "SUCCESS")]);
    let held = run_unstable(
        &unstable_policy(Some(&["zizmor (report only)"])),
        &json,
        &json,
    );
    assert_eq!(
        held.decision,
        StewardDecision::WaitingRequired {
            contexts: vec![
                UNSTABLE_WAIT.to_owned(),
                "unstable-without-a-visible-non-green-check".to_owned(),
            ]
        }
    );
    assert_no_write(&held);
}

/// A held head reports each advisory entry that matches no check on it, so
/// a typo or a renamed job is visible; it admits nothing.
#[test]
fn an_advisory_entry_matching_no_check_is_reported_on_a_held_head() {
    let json = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "FAILURE")]);
    let mut pr = parse_pr(
        &serde_json::from_str(&json).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR")
    .fact;
    pr.checks.push(
        crate::app::merge_steward_cmd::observation::parse_rest_check(
            &serde_json::from_str(&rest_check_run(true)).expect("check run"),
        )
        .expect("check run"),
    );
    assert_eq!(
        classify_pr(
            &pr,
            &unstable_policy(Some(&["zizmor(report only)"])),
            &BTreeMap::new()
        ),
        StewardDecision::WaitingRequired {
            contexts: vec![
                UNSTABLE_WAIT.to_owned(),
                "zizmor (report only) (not in unstable_advisory_checks)".to_owned(),
                "unstable_advisory_checks entry matches no check: zizmor(report only)".to_owned(),
            ]
        }
    );
}

const UNSTABLE_WAIT: &str = "github-merge-state:CLEAN (current=UNSTABLE)";

/// With the list known, a head GitHub still reports UNSTABLE after the clear,
/// red only in an advisory check, merges in the same pass.
#[cfg(unix)]
#[test]
fn a_head_still_unstable_after_the_clear_merges_when_only_advisory_checks_are_red() {
    let (report, calls) = run_recovery_clear_pass_under(
        "UNSTABLE",
        &unstable_policy(Some(&["zizmor (report only)"])),
        &[("zizmor (report only)", "FAILURE")],
    );
    assert!(report.error.is_none(), "{report:?}\n{calls}");
    assert_eq!(report.decision, StewardDecision::ArmMergeQueue, "{calls}");
    assert_eq!(
        report.mutation,
        Some(format!(
            "needs_agent_cleared,merged_exact_head:{HEAD},advisory_not_green:zizmor_(report_only)"
        )),
        "{calls}"
    );
    let clear = calls.find("state=success").expect("recovery clear");
    let merge = calls.find("mergePullRequest(").expect("merge");
    assert!(clear < merge, "{calls}");
}

/// Two producers report an advisory-named check: each is judged on its own
/// newest observation, and a rerun by the same producer supersedes its own
/// earlier attempt.
#[test]
fn each_producer_of_a_check_is_judged_on_its_own_newest_observation() {
    let policy = unstable_policy(Some(&["zizmor (report only)"]));
    let classify = |second_app: Option<u64>, second: &str, extra_unlisted_app: Option<u64>| {
        let mut pr = parse_pr(
            &serde_json::from_str(&unstable_pr_json("SUCCESS", &[])).expect("PR json"),
            &BTreeMap::new(),
        )
        .expect("PR")
        .fact;
        pr.checks.push(
            crate::app::merge_steward_cmd::observation::parse_rest_check(
                &serde_json::from_str(&rest_check_run(true)).expect("check run"),
            )
            .expect("check run"),
        );
        let base = StewardCheck {
            name: "risk-tier".to_owned(),
            source: crate::merge_steward::StewardCheckSource::CheckRun,
            app_id: Some(ACTIONS_APP),
            status: "COMPLETED".to_owned(),
            conclusion: Some("FAILURE".to_owned()),
            run_id: Some(500),
            observed_at: Some("2026-10-30T00:00:10Z".to_owned()),
            check_id: Some(5000),
            description: None,
        };
        pr.checks.push(base.clone());
        pr.checks.push(StewardCheck {
            app_id: second_app,
            conclusion: Some(second.to_owned()),
            run_id: Some(501),
            observed_at: Some("2026-10-30T00:00:20Z".to_owned()),
            check_id: Some(5001),
            ..base.clone()
        });
        if let Some(app) = extra_unlisted_app {
            pr.checks.push(StewardCheck {
                name: "zizmor (report only)".to_owned(),
                app_id: Some(app),
                run_id: Some(502),
                check_id: Some(5002),
                ..base
            });
        }
        classify_pr(&pr, &policy, &BTreeMap::new())
    };
    let held = StewardDecision::WaitingRequired {
        contexts: vec![
            UNSTABLE_WAIT.to_owned(),
            "risk-tier (not in unstable_advisory_checks)".to_owned(),
        ],
    };
    // Same producer, green rerun: risk-tier is green; only the advisory
    // zizmor is red, so it merges.
    assert_eq!(
        classify(Some(ACTIONS_APP), "SUCCESS", Some(ACTIONS_APP)),
        StewardDecision::ArmMergeQueue
    );
    // Another producer's green does not hide the red attempt.
    assert_eq!(classify(Some(99), "SUCCESS", Some(ACTIONS_APP)), held);
}

/// A check name with a newline or spaces stays one token in the mutation.
#[cfg(unix)]
#[test]
fn a_hostile_check_name_cannot_split_the_mutation_or_summary_line() {
    let name = "evil\\nok=1, x;y|z";
    let json = unstable_pr_json("SUCCESS", &[(name, "FAILURE")]);
    let outcome = run_unstable(&unstable_policy(Some(&["evil\nok=1, x;y|z"])), &json, &json);
    assert_merged(&outcome);
    let mutation = outcome.mutation.expect("mutation");
    assert!(
        !mutation.contains('\n') && !mutation.contains(' '),
        "{mutation:?}"
    );
    assert_eq!(mutation.matches(',').count(), 1, "{mutation:?}");
}

/// The final revalidation under the merge guard holds on its own: the census
/// and the pre-guard read are mergeable, and only the final read sees an
/// unlisted check red, so nothing merges. Its re-classification refuses
/// first; the explicit merge-state term after it re-checks the same read.
#[cfg(unix)]
#[test]
fn the_final_live_read_alone_refuses_an_unlisted_red_check() {
    let policy = unstable_policy(Some(&["zizmor (report only)"]));
    let green = unstable_pr_json("SUCCESS", &[("zizmor (report only)", "FAILURE")]);
    // Control: green on every read merges, after two live reads.
    let merged = run_unstable_reads(&policy, &green, &green, &green);
    assert_merged(&merged);
    let reads = merged
        .calls
        .lines()
        .filter(|line| line.starts_with("pr view "))
        .count();
    assert_eq!(reads, 2, "{}", merged.calls);

    let unlisted_red = unstable_pr_json(
        "SUCCESS",
        &[
            ("zizmor (report only)", "FAILURE"),
            ("risk-tier", "FAILURE"),
        ],
    );
    let held = run_unstable_reads(&policy, &green, &green, &unlisted_red);
    assert_eq!(held.decision, StewardDecision::ArmMergeQueue);
    assert!(
        held.mutation
            .as_deref()
            .is_some_and(|m| m.contains("skipped_after_final_live_revalidation")),
        "{:?}\n{}",
        held.mutation,
        held.calls
    );
    assert_no_write(&held);
}
