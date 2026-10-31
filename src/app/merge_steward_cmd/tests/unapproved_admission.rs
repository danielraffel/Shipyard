//! The unapproved-admission sweep: on every pass, on every merge path, an
//! armed native auto-merge request or a merge-queue entry whose head the
//! exact-head approval gate does not admit is withdrawn, whoever owns the pull
//! request. The fake `gh` runs the real `ghapp` queue-removal guard on each
//! removal, as the wrapper would.
use super::*;
use crate::app::merge_steward_cmd::cancellation::apply_pr_plans;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const OLD_HEAD: &str = "ca1f305c00000000000000000000000000000000";
const MOVED: &str = "cccccccccccccccccccccccccccccccccccccccc";
const ACTIONS_APP: u64 = 15368;

const POLICY_REQUIRED: &str = r#"printf '%s\n' '[auto_merge]' 'arm_requires_head_approval = true' 'reviewer_identities = ["shipyard-review[bot]"]'"#;

fn guard_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/ghapp_queue_removal_guard.py")
}

fn approval_for(head: &str) -> String {
    format!(
        r#"{{"id":12345,"login":"shipyard-review[bot]","type":"Bot","at":"2026-10-30T12:00:00Z","body":"LGTM\nreviewed: {head}","performed_via_github_app_id":99}}"#
    )
}

/// The pull request row as GitHub reports it: open, unmanaged, `macos` green.
fn pr_row(armed: bool) -> String {
    pr_row_numbered(42, armed)
}

fn pr_row_numbered(number: u64, armed: bool) -> String {
    let auto_merge = if armed {
        r#"{"enabledAt":"2026-10-28T22:42:00Z"}"#
    } else {
        "null"
    };
    format!(
        r#"{{"id":"PR_kw{number}","number":{number},"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":{auto_merge},"labels":[{{"name":"shipyard:unmanaged"}}],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}"#
    )
}

/// The census row: armed, or (with its arm consumed) in the merge queue.
fn census_pr(number: u64, queued: bool) -> ObservedPr {
    let positions = if queued {
        BTreeMap::from([(number, 1)])
    } else {
        BTreeMap::new()
    };
    let pr = parse_pr(
        &serde_json::from_str(&pr_row_numbered(number, !queued)).expect("PR json"),
        &positions,
    )
    .expect("PR");
    assert_eq!(pr.fact.auto_merge_active, !queued);
    assert_eq!(pr.fact.queue_position.is_some(), queued);
    pr
}

#[derive(Clone, Copy, Debug)]
enum Admission {
    NativeQueue,
    NoQueue,
    /// No queue and no app-bound approval rule:
    /// `direct_merge_refused_server_enforcement_required`. A surviving arm
    /// there merges the head directly.
    Refused,
}

fn policy_for(path: Admission) -> StewardPolicy {
    let macos = RequiredCheck {
        context: "macos".to_owned(),
        app_id: None,
    };
    match path {
        Admission::NativeQueue => queue_policy(),
        Admission::NoQueue => StewardPolicy {
            merge_queue: false,
            required_checks: vec![
                macos,
                RequiredCheck {
                    context: "review-verdict".to_owned(),
                    app_id: Some(ACTIONS_APP),
                },
            ],
            ..queue_policy()
        },
        Admission::Refused => StewardPolicy {
            merge_queue: false,
            required_checks: vec![macos],
            ..queue_policy()
        },
    }
}

#[allow(clippy::struct_excessive_bools)] // Independent fixture switches.
struct Fixture {
    path: Admission,
    /// Issue comments the approval gate reads.
    comments: String,
    /// Whether the reviewer-policy read fails (approval unreadable).
    policy_unreadable: bool,
    /// Whether a successful read finds no `.shipyard/config.toml`.
    policy_absent: bool,
    /// Whether the live queue-state read fails.
    queue_unreadable: bool,
    /// A 200 queue-state answer that names no pull request (GraphQL
    /// `errors`, or `pullRequest: null`).
    queue_body: Option<&'static str>,
    /// Whether the approval reviews read fails.
    reviews_unreadable: bool,
    /// Queue removals of the head in the timeline, after the approval.
    removals: usize,
    /// Whether the approval comments read fails.
    comments_unreadable: bool,
    /// The repository observed.
    repo: &'static str,
    /// Head the live queue-state read reports.
    live_head: &'static str,
    apply: bool,
    /// In the merge queue (arm consumed) rather than armed.
    queued: bool,
    /// The steward owns the PR (classify-all policy) instead of declining it.
    owned: bool,
    /// Open pull requests: #42, then #43, ... each in the same state.
    prs: u64,
}

impl Fixture {
    fn unapproved(path: Admission) -> Self {
        Self {
            path,
            comments: approval_for(OLD_HEAD),
            policy_unreadable: false,
            policy_absent: false,
            queue_unreadable: false,
            queue_body: None,
            reviews_unreadable: false,
            removals: 0,
            comments_unreadable: false,
            repo: "owner/repo",
            live_head: HEAD,
            apply: true,
            queued: false,
            owned: false,
            prs: 1,
        }
    }

    fn approved(path: Admission) -> Self {
        Self {
            comments: approval_for(HEAD),
            ..Self::unapproved(path)
        }
    }

    fn script(&self, log: &Path) -> String {
        let fail = r#"echo "HTTP 502: Bad Gateway" >&2; exit 1"#;
        let policy = if self.policy_unreadable {
            fail.to_owned()
        } else if self.policy_absent {
            r#"echo "gh: Not Found (HTTP 404)" >&2; exit 1"#.to_owned()
        } else {
            POLICY_REQUIRED.to_owned()
        };
        let comments = if self.comments_unreadable {
            fail.to_owned()
        } else {
            format!("printf '%s\\n' '{}'", self.comments)
        };
        let (in_queue, entry, auto_merge) = if self.queued {
            ("true", r#"{"state":"QUEUED","position":1}"#, "null")
        } else {
            ("false", "null", r#"{"enabledAt":"2026-10-28T22:42:00Z"}"#)
        };
        format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) printf 'policy-read\n' >> '{log}'; {policy} ;;
  *"disablePullRequestAutoMerge"*)
    printf 'marker=%s\n' "${{SHIPYARD_INTERNAL_QUEUE_MUTATION:-}}" >> '{log}'
    # The real queue-removal guard judges the call, as `ghapp` would.
    python3 -I '{guard}' "$@" 2>> '{log}' || exit 97
    printf '%s' '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":42,"autoMergeRequest":null}}}}}}}}' ;;
  *"dequeuePullRequest"*)
    printf 'marker=%s\n' "${{SHIPYARD_INTERNAL_QUEUE_MUTATION:-}}" >> '{log}'
    python3 -I '{guard}' "$@" 2>> '{log}' || exit 97
    printf '%s' '{{"data":{{"dequeuePullRequest":{{"mergeQueueEntry":{{"id":"MQE_1"}}}}}}}}' ;;
  *"enablePullRequestAutoMerge"*) echo "the sweep must never arm" >&2; exit 92 ;;
  *"mergePullRequest("*) echo "the sweep must never merge" >&2; exit 93 ;;
  *"enqueuePullRequest"*) echo "the sweep must never enqueue" >&2; exit 91 ;;
  *"/reviews"*) {reviews} ;;
  *"/labels"*) printf '{{}}' ;;
  *"/pulls/4"[0-9]" "*) printf 'feature\nmain\n' ;;
  *"query=query("*"mergeQueue(branch"*) printf '%s' '{merge_queue}' ;;
  *"/comments"*) {comments} ;;
  *"commits/{HEAD}"*) printf '%s' '2026-10-30T00:00:00Z' ;;
  *"isInMergeQueue"*) {queue_fail}printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":{in_queue},"mergeQueueEntry":{entry},"autoMergeRequest":{auto_merge},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[{removals}]}}}}}}}}}}' ;;
  "pr view "*) printf '%s' '{live_pr}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            guard = guard_script().display(),
            comments = comments,
            reviews = if self.reviews_unreadable { fail } else { "printf ''" },
            removals = (0..self.removals)
                .map(|minute| format!(
                    r#"{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-10-30T13:0{minute}:00Z","reason":"MANUAL","actor":{{"login":"shipyard-local[bot]"}},"beforeCommit":{{"oid":"{HEAD}","parents":{{"nodes":[]}}}}}}"#
                ))
                .collect::<Vec<_>>()
                .join(","),
            queue_fail = if let Some(body) = self.queue_body {
                format!("printf '%s' '{body}'; exit 0; ")
            } else if self.queue_unreadable {
                format!("{fail}; ")
            } else {
                String::new()
            },
            live_head = self.live_head,
            live_pr = pr_row(false),
            merge_queue = match self.path {
                Admission::NativeQueue =>
                    r#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#,
                Admission::NoQueue | Admission::Refused =>
                    r#"{"data":{"repository":{"mergeQueue":null}}}"#,
            },
        )
    }
}

struct Outcome {
    /// Whether the pass reported itself unhealthy.
    unhealthy: bool,
    reports: Vec<PrReport>,
    report: PrReport,
    calls: String,
    ledger: StewardLedger,
}

fn run(fixture: &Fixture) -> Outcome {
    run_with(fixture, StewardLedger::default())
}

/// One steward pass over `fixture`, carrying `ledger` from earlier passes.
fn run_with(fixture: &Fixture, mut ledger: StewardLedger) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(&temp, &fixture.script(&log));
    let policy = policy_for(fixture.path);
    let mut observation = observation_for(
        census_pr(42, fixture.queued),
        matches!(fixture.path, Admission::NativeQueue),
    );
    observation.prs = (0..fixture.prs)
        .map(|offset| census_pr(42 + offset, fixture.queued))
        .collect();
    observation.repo = fixture.repo.to_owned();
    observation.allow_auto_merge = true;
    observation.required_checks = policy.required_checks.clone();
    let args = StewardCommandArgs {
        repos: vec![fixture.repo.to_owned()],
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
        apply: fixture.apply,
        ledger: None,
    };
    // `managed_label: None` is the legacy classify-all policy: every PR owned.
    let policy = if fixture.owned {
        policy
    } else {
        StewardPolicy {
            managed_label: Some(MANAGED_LABEL.to_owned()),
            handoff_context: HANDOFF_CONTEXT.to_owned(),
            ..policy
        }
    };
    let control = mutation_control(&temp, "studio", "studio");
    let (reports, unhealthy) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &policy,
        &temp.path().join("ledger.json"),
        &mut ledger,
        fixture.apply.then_some(&control),
    );
    Outcome {
        unhealthy,
        report: reports[0].clone(),
        reports,
        calls: fs::read_to_string(log).unwrap_or_default(),
        ledger,
    }
}

/// The withdrawal ran on Shipyard's audited internal path and the real
/// removal guard accepted it.
fn assert_guard_accepted(outcome: &Outcome) {
    assert!(outcome.calls.contains("marker=1"), "{}", outcome.calls);
    assert!(
        !outcome.calls.contains("refusing"),
        "the removal guard refused the withdrawal:\n{}",
        outcome.calls
    );
}

fn assert_audited(outcome: &Outcome, action: &str) {
    assert!(
        outcome
            .ledger
            .audit
            .iter()
            .any(|entry| entry.action == action && entry.subject == format!("pr:42:{HEAD}")),
        "no ledger audit {action}"
    );
}

/// Later reads for an unmanaged PR (labels, recovery) are not served by this
/// fixture; only the withdrawal's own outcome is under test.
fn assert_no_withdrawal_error(outcome: &Outcome) {
    assert!(
        !outcome.report.error.as_deref().is_some_and(|error| {
            error.contains("auto-merge") || error.contains("disarm") || error.contains("dequeue")
        }),
        "{:?}",
        outcome.report
    );
}

fn assert_disarmed(outcome: &Outcome) {
    assert_eq!(
        outcome.report.mutation.as_deref(),
        Some(format!("disarmed_stale_auto_merge:{HEAD}").as_str()),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    assert_no_withdrawal_error(outcome);
    assert_guard_accepted(outcome);
    assert_audited(outcome, "disarm_stale_auto_merge");
}

fn assert_dequeued(outcome: &Outcome) {
    // The sweep's own mutation comes first; an owned PR's later steps may
    // append theirs.
    assert_eq!(
        outcome
            .report
            .mutation
            .as_deref()
            .and_then(|mutation| mutation.split(',').next()),
        Some(format!("dequeued_unapproved:{HEAD}").as_str()),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    assert_no_withdrawal_error(outcome);
    assert_guard_accepted(outcome);
    assert_audited(outcome, "dequeue_unapproved_queue_entry");
    assert!(
        !outcome.calls.contains("disablePullRequestAutoMerge"),
        "{}",
        outcome.calls
    );
}

fn assert_left_alone(outcome: &Outcome) {
    for write in ["disablePullRequestAutoMerge", "dequeuePullRequest"] {
        assert!(!outcome.calls.contains(write), "{write}\n{}", outcome.calls);
    }
    assert!(
        !outcome.report.mutation.as_deref().is_some_and(|mutation| {
            mutation.contains("disarm") || mutation.contains("dequeue")
        }),
        "{:?}",
        outcome.report
    );
}

/// Replays #11117 before GitHub enqueued it: an arm enabled for an older head
/// survived a push by an agent with write access, and the new head carries no
/// approval.
#[test]
fn an_armed_unapproved_head_on_the_queue_path_is_disarmed() {
    let outcome = run(&Fixture::unapproved(Admission::NativeQueue));
    assert_eq!(outcome.report.decision, StewardDecision::Unmanaged);
    assert_disarmed(&outcome);
}

#[test]
fn an_armed_approved_head_on_the_queue_path_is_left_alone() {
    assert_left_alone(&run(&Fixture::approved(Admission::NativeQueue)));
}

#[test]
fn an_unreadable_approval_disarms_on_the_queue_path() {
    assert_disarmed(&run(&Fixture {
        policy_unreadable: true,
        ..Fixture::unapproved(Admission::NativeQueue)
    }));
}

/// The gate approved the census head, but the armed request follows the live
/// head, which nobody has judged.
#[test]
fn a_head_that_moved_after_the_census_is_disarmed() {
    assert_disarmed(&run(&Fixture {
        live_head: MOVED,
        ..Fixture::approved(Admission::NativeQueue)
    }));
}

/// Replays #11117 as it was found: GitHub consumed the arm when it enqueued
/// the unapproved head, so there is no arm left to disarm; the entry itself is
/// removed.
#[test]
fn an_enqueued_unapproved_head_is_dequeued() {
    assert_dequeued(&run(&Fixture {
        queued: true,
        ..Fixture::unapproved(Admission::NativeQueue)
    }));
}

#[test]
fn an_enqueued_approved_head_is_left_in_the_queue() {
    assert_left_alone(&run(&Fixture {
        queued: true,
        ..Fixture::approved(Admission::NativeQueue)
    }));
}

fn deferred(outcome: &Outcome, head: &str) {
    assert_eq!(
        outcome.report.mutation.as_deref(),
        Some(format!("dequeue_deferred_unreadable:{head}").as_str()),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    // Visible: the read error is the PR's error, and the pass is unhealthy.
    assert!(
        outcome
            .report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("dequeue deferred")),
        "{:?}",
        outcome.report
    );
    assert!(outcome.unhealthy, "{:?}", outcome.report);
    assert_left_alone_writes(outcome);
}

/// Control on the same instrument: a readable, approved queued head leaves
/// the pass healthy, so `unhealthy` above is the deferral's doing.
#[test]
fn a_readable_approved_queued_head_leaves_the_pass_healthy() {
    let outcome = run(&queued_approved());
    assert_left_alone(&outcome);
    assert!(
        !outcome.unhealthy,
        "{:?}\n{}",
        outcome.report, outcome.calls
    );
    assert_eq!(outcome.report.error, None);
}

fn assert_streak_dequeued(outcome: &Outcome) {
    assert_eq!(
        outcome.report.mutation.as_deref(),
        Some(format!("dequeued_unreadable_streak:{HEAD}").as_str()),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    assert_guard_accepted(outcome);
    assert_audited(outcome, "dequeue_unapproved_queue_entry");
}

/// PR #42's unreadable streak in `owner/repo`, as `(head, passes)`.
fn streak(outcome: &Outcome) -> Option<(String, u32)> {
    let mut streaks = outcome
        .ledger
        .dequeue_unreadable
        .iter()
        .filter_map(|(key, passes)| {
            key.strip_prefix("owner/repo#42:")
                .map(|head| (head.to_owned(), *passes))
        });
    let found = streaks.next();
    assert!(streaks.next().is_none(), "one streak per PR");
    found
}

const GRAPHQL_ERRORS: &str =
    r#"{"data":{"repository":{"pullRequest":null}},"errors":[{"message":"Something went wrong"}]}"#;
const NULL_PULL_REQUEST: &str = r#"{"data":{"repository":{"pullRequest":null}}}"#;
/// GraphQL `errors` beside partial data that, read at face value, shows the
/// entry queued at a head nobody approved.
const GRAPHQL_ERRORS_WITH_DATA: &str = r#"{"data":{"repository":{"pullRequest":{"number":42,"state":"OPEN","headRefOid":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","isInMergeQueue":true,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{"pageInfo":{"hasPreviousPage":false},"nodes":[]}}}},"errors":[{"message":"timeline partially unavailable"}]}"#;

/// Every way a queued entry's judgement can be unreadable: policy 502, the
/// queue-state read failing, a 200 queue-state answer with GraphQL `errors` or
/// `pullRequest: null`, and the reviews or comments read failing.
fn each_failure(base: impl Fn() -> Fixture) -> Vec<(&'static str, Fixture)> {
    vec![
        (
            "policy 502",
            Fixture {
                policy_unreadable: true,
                ..base()
            },
        ),
        (
            "queue-state failure",
            Fixture {
                queue_unreadable: true,
                ..base()
            },
        ),
        (
            "GraphQL errors body",
            Fixture {
                queue_body: Some(GRAPHQL_ERRORS),
                ..base()
            },
        ),
        (
            "null pullRequest body",
            Fixture {
                queue_body: Some(NULL_PULL_REQUEST),
                ..base()
            },
        ),
        (
            "reviews failure",
            Fixture {
                reviews_unreadable: true,
                ..base()
            },
        ),
        (
            "comments failure",
            Fixture {
                comments_unreadable: true,
                ..base()
            },
        ),
    ]
}

fn queued_unapproved() -> Fixture {
    Fixture {
        queued: true,
        ..Fixture::unapproved(Admission::NativeQueue)
    }
}

fn queued_approved() -> Fixture {
    Fixture {
        queued: true,
        ..Fixture::approved(Admission::NativeQueue)
    }
}

/// A dequeue throws away a merge group's runs, so an unreadable read defers
/// it; only the third consecutive unreadable pass on the same head dequeues,
/// carried by the shared steward ledger.
#[test]
fn an_unreadable_queued_head_defers_twice_then_dequeues() {
    for (failure, fixture) in each_failure(queued_unapproved) {
        let first = run(&fixture);
        deferred(&first, HEAD);
        assert_eq!(streak(&first), Some((HEAD.to_owned(), 1)), "{failure}");
        let second = run_with(&fixture, first.ledger);
        deferred(&second, HEAD);
        assert_eq!(streak(&second), Some((HEAD.to_owned(), 2)), "{failure}");
        let third = run_with(&fixture, second.ledger);
        assert_streak_dequeued(&third);
        assert_eq!(streak(&third), None, "{failure}");
    }
}

/// A 200 answer with `errors` or no pull request is an unread state, not a
/// moved or unqueued head: it defers, never dequeues on the first pass.
#[test]
fn a_graphql_answer_without_a_pull_request_defers() {
    for body in [GRAPHQL_ERRORS, NULL_PULL_REQUEST, GRAPHQL_ERRORS_WITH_DATA] {
        deferred(
            &run(&Fixture {
                queue_body: Some(body),
                ..queued_unapproved()
            }),
            HEAD,
        );
    }
}

#[test]
fn an_unreadable_approval_defers_rather_than_dequeues() {
    deferred(
        &run(&Fixture {
            comments_unreadable: true,
            ..queued_unapproved()
        }),
        HEAD,
    );
}

#[test]
fn a_config_read_error_defers() {
    deferred(
        &run(&Fixture {
            policy_unreadable: true,
            ..queued_unapproved()
        }),
        HEAD,
    );
}

/// A 404 on the config is a fact a successful read proved, not an
/// unreadable policy.
#[test]
fn a_missing_config_after_a_successful_read_dequeues() {
    assert_dequeued(&run(&Fixture {
        policy_absent: true,
        ..queued_unapproved()
    }));
}

#[test]
fn a_successful_read_resets_the_streak() {
    let fixture = Fixture {
        comments_unreadable: true,
        ..queued_unapproved()
    };
    let first = run(&fixture);
    let second = run_with(&fixture, first.ledger);
    assert_eq!(streak(&second), Some((HEAD.to_owned(), 2)));
    // A successful read of an approved head: kept, and the streak clears.
    let readable = run_with(&queued_approved(), second.ledger);
    assert_left_alone(&readable);
    assert_eq!(streak(&readable), None);
    // So the next unreadable pass starts over rather than dequeuing.
    let again = run_with(&fixture, readable.ledger);
    deferred(&again, HEAD);
    assert_eq!(streak(&again), Some((HEAD.to_owned(), 1)));
}

#[test]
fn a_new_head_resets_the_streak() {
    let fixture = Fixture {
        comments_unreadable: true,
        ..queued_unapproved()
    };
    let first = run(&fixture);
    let second = run_with(&fixture, first.ledger);
    assert_eq!(streak(&second), Some((HEAD.to_owned(), 2)));
    let moved = run_with(
        &Fixture {
            live_head: MOVED,
            ..fixture
        },
        second.ledger,
    );
    deferred(&moved, MOVED);
    assert_eq!(streak(&moved), Some((MOVED.to_owned(), 1)));
}

/// At the start of an applying pass, a streak survives only while its pull
/// request is still queued at that head, and a steward-removal record only
/// while the pull request is open at that head.
#[test]
fn the_pass_prunes_streaks_for_unqueued_or_moved_pull_requests() {
    let key = |number: u64, head: &str| format!("owner/repo#{number}:{head}");
    let mut ledger = StewardLedger::default();
    for number in [42, 43, 44, 45] {
        ledger.dequeue_unreadable.insert(key(number, HEAD), 2);
        ledger.steward_dequeues.insert(key(number, HEAD), 1);
    }
    ledger
        .dequeue_unreadable
        .insert("owner/other#42:x".to_owned(), 2);
    let mut observation = observation_for(census_pr(42, true), true);
    let mut moved = census_pr(44, true);
    moved.fact.head_sha = MOVED.to_owned();
    // 42 queued at HEAD; 43 open but unqueued; 44 queued at a new head; 45 gone.
    observation.prs = vec![census_pr(42, true), census_pr(43, false), moved];
    crate::app::merge_steward_cmd::unapproved_admission::prune(&mut ledger, &observation);
    assert_eq!(
        ledger
            .dequeue_unreadable
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        ["owner/other#42:x".to_owned(), key(42, HEAD)]
    );
    assert_eq!(
        ledger.steward_dequeues.keys().cloned().collect::<Vec<_>>(),
        [key(42, HEAD), key(43, HEAD)]
    );
}

/// The streak is cleared when a judgement finds the pull request unqueued.
#[test]
fn an_unqueued_pull_request_loses_its_streak() {
    let fixture = Fixture {
        comments_unreadable: true,
        ..queued_unapproved()
    };
    let first = run(&fixture);
    assert_eq!(streak(&first), Some((HEAD.to_owned(), 1)));
    let unqueued = run_with(&Fixture::approved(Admission::NativeQueue), first.ledger);
    assert_eq!(streak(&unqueued), None);
}

/// An approved head is never dequeued with each failure injected: the
/// unreadable passes defer, and the readable pass that shows the approval
/// keeps it and clears the streak before it can reach the bound.
#[test]
fn an_approved_head_is_never_dequeued_with_each_failure_injected() {
    for (failure, failing) in each_failure(queued_approved) {
        let mut ledger = StewardLedger::default();
        for (pass, fixture) in [&failing, &failing, &queued_approved(), &failing, &failing]
            .into_iter()
            .enumerate()
        {
            let outcome = run_with(fixture, ledger);
            assert!(
                !outcome.calls.contains("dequeuePullRequest"),
                "{failure}, pass {pass}: {}",
                outcome.calls
            );
            ledger = outcome.ledger;
        }
        assert_eq!(
            ledger
                .dequeue_unreadable
                .values()
                .copied()
                .collect::<Vec<_>>(),
            [2],
            "{failure}"
        );
    }
}

/// A circuit breaker: at most two entries leave the queue per repository and
/// base per pass; the rest report `dequeue_deferred_cap`. Two repositories
/// with three candidates each give two and two.
#[test]
fn at_most_two_dequeues_per_repository_per_pass() {
    let mut ledger = StewardLedger::default();
    for repo in ["owner/repo", "owner/other"] {
        let outcome = run_with(
            &Fixture {
                prs: 3,
                repo,
                ..queued_unapproved()
            },
            ledger,
        );
        let mutations = outcome
            .reports
            .iter()
            .map(|report| report.mutation.clone().unwrap_or_default())
            .collect::<Vec<_>>();
        assert_eq!(
            mutations,
            [
                format!("dequeued_unapproved:{HEAD}"),
                format!("dequeued_unapproved:{HEAD}"),
                format!("dequeue_deferred_cap:{HEAD}"),
            ],
            "{repo}\n{}",
            outcome.calls
        );
        assert_eq!(outcome.calls.matches("dequeuePullRequest").count(), 2);
        assert_guard_accepted(&outcome);
        ledger = outcome.ledger;
    }
}

/// A steward dequeue does not consume the head's ejection cap: two steward
/// removals of an approved head (three unreadable passes each, say) leave it
/// admitted, where two removals by anyone else would not.
#[test]
fn a_steward_dequeue_does_not_consume_the_ejection_cap() {
    let fixture = Fixture {
        removals: crate::head_approval::EJECTION_CAP,
        ..queued_approved()
    };
    // Control: the same two removals, not the steward's, cap the head.
    let foreign = run(&fixture);
    assert_dequeued(&foreign);
    // The steward recorded both removals as its own.
    let mut ledger = StewardLedger::default();
    ledger
        .steward_dequeues
        .insert(format!("owner/repo#42:{HEAD}"), 2);
    let own = run_with(&fixture, ledger);
    assert_left_alone(&own);
}

/// The steward records each of its own removals for that head.
#[test]
fn a_steward_dequeue_is_recorded_as_its_own() {
    let outcome = run(&queued_unapproved());
    assert_dequeued(&outcome);
    assert_eq!(
        outcome
            .ledger
            .steward_dequeues
            .get(&format!("owner/repo#42:{HEAD}")),
        Some(&1)
    );
}

/// The steward's own entries are untouched only because their heads are
/// approved; owning a queued PR does not exempt an unapproved entry.
#[test]
fn an_owned_enqueued_unapproved_head_is_dequeued() {
    assert_dequeued(&run(&Fixture {
        queued: true,
        owned: true,
        ..Fixture::unapproved(Admission::NativeQueue)
    }));
}

#[test]
fn an_armed_unapproved_unowned_head_on_the_no_queue_path_is_disarmed() {
    let outcome = run(&Fixture::unapproved(Admission::NoQueue));
    assert_eq!(
        policy_for(Admission::NoQueue).merge_path(),
        "server_enforced_exact_head_merge"
    );
    assert_disarmed(&outcome);
}

#[test]
fn an_armed_approved_unowned_head_on_the_no_queue_path_is_left_alone() {
    assert_left_alone(&run(&Fixture::approved(Admission::NoQueue)));
}

#[test]
fn an_unreadable_approval_disarms_on_the_no_queue_path() {
    assert_disarmed(&run(&Fixture {
        policy_unreadable: true,
        ..Fixture::unapproved(Admission::NoQueue)
    }));
}

/// A base the steward refuses to merge on is still swept: a surviving arm
/// there merges the head directly.
#[test]
fn a_refused_base_is_swept_too() {
    assert_eq!(
        policy_for(Admission::Refused).merge_path(),
        "direct_merge_refused_server_enforcement_required"
    );
    assert_disarmed(&run(&Fixture::unapproved(Admission::Refused)));
    assert_left_alone(&run(&Fixture::approved(Admission::Refused)));
}

/// The steward admits an owned head itself, so an arm on one is disarmed on
/// the queue path too, approved or not.
#[test]
fn an_owned_armed_approved_head_on_the_queue_path_is_disarmed() {
    let outcome = run(&Fixture {
        owned: true,
        ..Fixture::approved(Admission::NativeQueue)
    });
    assert!(
        outcome
            .report
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.contains(&format!("auto_merge_disabled:{HEAD}"))),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    assert_guard_accepted(&outcome);
    assert_audited(&outcome, "disable_auto_merge_owned_pr");
}

#[test]
fn the_reviewer_policy_is_read_once_per_pass() {
    let outcome = run(&Fixture {
        prs: 3,
        ..Fixture::unapproved(Admission::NativeQueue)
    });
    assert_eq!(outcome.reports.len(), 3);
    for report in &outcome.reports {
        assert_eq!(
            report.mutation.as_deref(),
            Some(format!("disarmed_stale_auto_merge:{HEAD}").as_str()),
            "{report:?}\n{}",
            outcome.calls
        );
    }
    assert_eq!(
        outcome.calls.matches("policy-read").count(),
        1,
        "{}",
        outcome.calls
    );
}

#[test]
fn a_dry_run_lists_what_it_would_withdraw_and_changes_nothing() {
    for path in [
        Admission::NativeQueue,
        Admission::NoQueue,
        Admission::Refused,
    ] {
        let outcome = run(&Fixture {
            apply: false,
            ..Fixture::unapproved(path)
        });
        assert_eq!(
            outcome.report.mutation.as_deref(),
            Some(format!("would_disarm_stale_auto_merge:{HEAD}").as_str()),
            "{path:?} {:?}\n{}",
            outcome.report,
            outcome.calls
        );
        assert_left_alone_writes(&outcome);
    }
    let outcome = run(&Fixture {
        apply: false,
        queued: true,
        ..Fixture::unapproved(Admission::NativeQueue)
    });
    assert_eq!(
        outcome.report.mutation.as_deref(),
        Some(format!("would_dequeue_unapproved:{HEAD}").as_str()),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    assert_left_alone_writes(&outcome);
}

fn assert_left_alone_writes(outcome: &Outcome) {
    for write in ["disablePullRequestAutoMerge", "dequeuePullRequest"] {
        assert!(!outcome.calls.contains(write), "{write}\n{}", outcome.calls);
    }
}
