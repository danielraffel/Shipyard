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
    /// `[merge_steward] unapproved_dequeue`.
    dequeue_mode: crate::app::merge_steward_cmd::DequeueMode,
    /// Queued and still armed (with `queued`).
    armed_and_queued: bool,
    /// Whether the disarm mutation fails.
    disarm_fails: bool,
    /// The dequeue mutation's answer, when not the accepted one.
    dequeue_answer: Option<&'static str>,
    /// The census row carries no node ID.
    empty_node_id: bool,
    /// Extra labels on the pull request.
    labels: &'static [&'static str],
    /// The config read's stderr, when it fails in a particular way.
    policy_stderr: Option<&'static str>,
    /// Reason of the latest removal (with `removals`).
    latest_removal_reason: &'static str,
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
            dequeue_mode: crate::app::merge_steward_cmd::DequeueMode::Apply,
            armed_and_queued: false,
            disarm_fails: false,
            dequeue_answer: None,
            empty_node_id: false,
            labels: &[],
            policy_stderr: None,
            latest_removal_reason: "MANUAL",
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
        let policy = if let Some(stderr) = self.policy_stderr {
            format!("echo '{stderr}' >&2; exit 1")
        } else if self.policy_unreadable {
            fail.to_owned()
        } else if self.policy_absent {
            r#"echo "gh: Not Found (HTTP 404)" >&2; exit 1"#.to_owned()
        } else {
            POLICY_REQUIRED.to_owned()
        };
        // A failure particular to this PR's records, not to the transport.
        let comments = if self.comments_unreadable {
            r#"echo "gh: Validation Failed (HTTP 422)" >&2; exit 1"#.to_owned()
        } else {
            format!("printf '%s\\n' '{}'", self.comments)
        };
        let (in_queue, entry, auto_merge) = if self.queued && self.armed_and_queued {
            (
                "true",
                r#"{"state":"QUEUED","position":1}"#,
                r#"{"enabledAt":"2026-10-28T22:42:00Z"}"#,
            )
        } else if self.queued {
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
    {disarm_fail}printf '%s' '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":42,"autoMergeRequest":null}}}}}}}}' ;;
  *"dequeuePullRequest"*)
    printf 'marker=%s\n' "${{SHIPYARD_INTERNAL_QUEUE_MUTATION:-}}" >> '{log}'
    python3 -I '{guard}' "$@" 2>> '{log}' || exit 97
    printf '%s' '{dequeue_answer}' ;;
  *"enablePullRequestAutoMerge"*) echo "the sweep must never arm" >&2; exit 92 ;;
  *"mergePullRequest("*) echo "the sweep must never merge" >&2; exit 93 ;;
  *"enqueuePullRequest"*) echo "the sweep must never enqueue" >&2; exit 91 ;;
  *"/reviews"*) {reviews} ;;
  *"/labels"*) printf '{{}}' ;;
  *"/pulls/4"[0-9]" "*) printf 'feature\nmain\n' ;;
  *"query=query("*"mergeQueue(branch"*) printf '%s' '{merge_queue}' ;;
  *"/comments"*) {comments} ;;
  *"/commits/"*) printf '%s' '2026-10-30T00:00:00Z' ;;
  *"/branches/main"*) printf 'main\n' ;;
  *"contents/.shipyard?ref="*) echo "gh: Not Found (HTTP 404)" >&2; exit 1 ;;
  *"isInMergeQueue"*) {queue_fail}printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":{in_queue},"mergeQueueEntry":{entry},"autoMergeRequest":{auto_merge},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[{removals}]}}}}}}}}}}' ;;
  "pr view "*) printf '%s' '{live_pr}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            guard = guard_script().display(),
            comments = comments,
reviews = if self.reviews_unreadable {
                // The shared REST bucket exhausted: a systemic failure.
                r#"echo "gh: API rate limit exceeded for installation. (HTTP 403)" >&2; exit 1"#
            } else {
                "printf ''"
            },
            removals = (0..self.removals)
                .map(|minute| (minute, if minute + 1 == self.removals { self.latest_removal_reason } else { "MANUAL" }))
                .map(|(minute, reason)| format!(
                    r#"{{"__typename":"RemovedFromMergeQueueEvent","createdAt":"2026-10-30T13:0{minute}:00Z","reason":"{reason}","actor":{{"login":"shipyard-local[bot]"}},"beforeCommit":{{"oid":"{HEAD}","parents":{{"nodes":[]}}}}}}"#
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
            disarm_fail = if self.disarm_fails {
                "echo 'GraphQL: could not disable auto-merge' >&2; exit 1; "
            } else {
                ""
            },
            dequeue_answer = self.dequeue_answer.unwrap_or(
                r#"{"data":{"dequeuePullRequest":{"mergeQueueEntry":{"id":"MQE_1"}}}}"#
            ),
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
        .map(|offset| {
            let mut pr = census_pr(42 + offset, fixture.queued);
            pr.fact.auto_merge_active |= fixture.armed_and_queued;
            if fixture.empty_node_id {
                pr.node_id = String::new();
            }
            pr.fact
                .labels
                .extend(fixture.labels.iter().map(|label| (*label).to_owned()));
            pr
        })
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
        unapproved_dequeue: fixture.dequeue_mode,
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

/// Failures particular to one pull request: a 200 queue-state answer with
/// GraphQL `errors` or `pullRequest: null`, and a 422 on its comments.
fn pr_specific_failures(base: impl Fn() -> Fixture) -> Vec<(&'static str, Fixture)> {
    vec![
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
            "comments 422",
            Fixture {
                comments_unreadable: true,
                ..base()
            },
        ),
    ]
}

/// Failures that would fail every pull request alike: the base policy (502),
/// the queue-state transport (502), and an exhausted REST bucket (403 rate
/// limit) on the reviews read.
fn systemic_failures(base: impl Fn() -> Fixture) -> Vec<(&'static str, Fixture)> {
    vec![
        (
            "policy 502",
            Fixture {
                policy_unreadable: true,
                ..base()
            },
        ),
        (
            "queue-state 502",
            Fixture {
                queue_unreadable: true,
                ..base()
            },
        ),
        (
            "reviews rate limit",
            Fixture {
                reviews_unreadable: true,
                ..base()
            },
        ),
    ]
}

fn each_failure(base: impl Fn() -> Fixture) -> Vec<(&'static str, Fixture)> {
    let mut failures = pr_specific_failures(&base);
    failures.extend(systemic_failures(&base));
    failures
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

/// A dequeue throws away a merge group's runs, so a PR-specific unreadable
/// read defers it; only the third consecutive one on the same head dequeues,
/// carried by the shared steward ledger, and says so.
#[test]
fn a_pr_specific_unreadable_head_defers_twice_then_dequeues() {
    for (failure, fixture) in pr_specific_failures(queued_unapproved) {
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

/// A base-wide or transport failure says nothing about one entry: it defers,
/// visibly, but never advances a streak, so an outage of any length cannot
/// drain the queue.
#[test]
fn a_systemic_failure_never_advances_the_streak() {
    for (failure, fixture) in systemic_failures(queued_unapproved) {
        let mut ledger = StewardLedger::default();
        for pass in 0..6 {
            let outcome = run_with(&fixture, ledger);
            deferred(&outcome, HEAD);
            assert_eq!(streak(&outcome), None, "{failure}, pass {pass}");
            ledger = outcome.ledger;
        }
    }
}

/// A systemic failure leaves an existing PR-specific streak where it was:
/// neither advanced nor cleared.
#[test]
fn a_systemic_failure_leaves_an_existing_streak_alone() {
    let specific = Fixture {
        comments_unreadable: true,
        ..queued_unapproved()
    };
    let first = run(&specific);
    let second = run_with(&specific, first.ledger);
    assert_eq!(streak(&second), Some((HEAD.to_owned(), 2)));
    let outage = run_with(
        &Fixture {
            policy_unreadable: true,
            ..queued_unapproved()
        },
        second.ledger,
    );
    deferred(&outage, HEAD);
    assert_eq!(streak(&outage), Some((HEAD.to_owned(), 2)));
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
/// request is still queued at that head, a steward-removal record only while
/// the pull request is open at that head, and the superseded count-only record
/// is dropped.
#[test]
fn the_pass_prunes_streaks_for_unqueued_or_moved_pull_requests() {
    let key = |number: u64, head: &str| format!("owner/repo#{number}:{head}");
    let mut ledger = StewardLedger::default();
    for number in [42, 43, 44, 45] {
        ledger.dequeue_unreadable.insert(key(number, HEAD), 2);
        ledger
            .steward_removals
            .insert(key(number, HEAD), vec!["2026-10-30T13:00:00Z".to_owned()]);
        ledger.steward_dequeues.insert(key(number, HEAD), 1);
    }
    ledger
        .dequeue_unreadable
        .insert("owner/other#42:x".to_owned(), 2);
    let mut moved = census_pr(44, true);
    moved.fact.head_sha = MOVED.to_owned();
    let mut observation = observation_for(census_pr(42, true), true);
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
        ledger.steward_removals.keys().cloned().collect::<Vec<_>>(),
        [key(42, HEAD), key(43, HEAD)]
    );
    assert!(ledger.steward_dequeues.is_empty());
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

/// An approved head is never dequeued with each failure injected: every
/// unreadable pass defers, and the readable pass that shows the approval
/// keeps it and clears any streak before it can reach the bound.
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
        assert!(
            ledger.dequeue_unreadable.values().all(|passes| *passes < 3),
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

/// A steward removal does not count toward the head's ejection cap, and only
/// the removals the steward recorded are excused. Judged on the arm, where
/// the cap still withdraws authority.
#[test]
fn a_steward_dequeue_does_not_consume_the_ejection_cap() {
    let fixture = Fixture {
        removals: crate::head_approval::EJECTION_CAP,
        ..Fixture::approved(Admission::NativeQueue)
    };
    // Control: two removals by someone else cap the head, so its arm goes.
    assert_disarmed(&run(&fixture));
    let recorded = |times: &[&str]| {
        let mut ledger = StewardLedger::default();
        ledger.steward_removals.insert(
            format!("owner/repo#42:{HEAD}"),
            times.iter().map(|time| (*time).to_owned()).collect(),
        );
        ledger
    };
    // Both removals were the steward's: the head keeps its arm.
    let own = run_with(
        &fixture,
        recorded(&["2026-10-30T13:00:30Z", "2026-10-30T13:01:10Z"]),
    );
    assert_left_alone(&own);
    // Two records, but one far from any removal: only one is excused, so
    // the other still counts and the cap still holds with a third removal.
    let partly = run_with(
        &Fixture {
            removals: crate::head_approval::EJECTION_CAP + 1,
            ..Fixture::approved(Admission::NativeQueue)
        },
        recorded(&["2026-10-30T13:00:30Z", "2026-10-30T18:00:00Z"]),
    );
    assert_disarmed(&partly);
}

/// The steward records when it removed each head.
#[test]
fn a_steward_dequeue_is_recorded_as_its_own() {
    let outcome = run(&queued_unapproved());
    assert_dequeued(&outcome);
    let times = outcome
        .ledger
        .steward_removals
        .get(&format!("owner/repo#42:{HEAD}"))
        .expect("recorded");
    assert_eq!(times.len(), 1);
    assert!(chrono::DateTime::parse_from_rfc3339(&times[0]).is_ok());
}

/// An approved head that reached the ejection cap stays queued: the cap
/// alone is never evidence for a dequeue, whatever the latest removal was.
#[test]
fn an_approved_head_at_the_ejection_cap_is_not_dequeued() {
    for reason in ["MANUAL", "FAILED_CHECKS"] {
        let outcome = run(&Fixture {
            removals: crate::head_approval::EJECTION_CAP,
            latest_removal_reason: reason,
            ..queued_approved()
        });
        assert_left_alone(&outcome);
    }
}

/// Without any approval, a capped head is dequeued on that evidence.
#[test]
fn an_unapproved_head_at_the_ejection_cap_is_dequeued() {
    assert_dequeued(&run(&Fixture {
        removals: crate::head_approval::EJECTION_CAP,
        ..queued_unapproved()
    }));
}

/// A dry run never advances the streak, even one pass short of the bound.
#[test]
fn a_dry_run_does_not_advance_the_streak() {
    let specific = Fixture {
        comments_unreadable: true,
        ..queued_unapproved()
    };
    let dry = Fixture {
        apply: false,
        ..specific
    };
    let first = run_with(&dry, StewardLedger::default());
    assert!(first.ledger.dequeue_unreadable.is_empty());
    let mut at_two = StewardLedger::default();
    at_two
        .dequeue_unreadable
        .insert(format!("owner/repo#42:{HEAD}"), 2);
    let dry_at_two = run_with(
        &Fixture {
            apply: false,
            comments_unreadable: true,
            ..queued_unapproved()
        },
        at_two,
    );
    assert_eq!(
        dry_at_two.report.mutation.as_deref(),
        Some(format!("would_dequeue_unreadable_streak:{HEAD}").as_str())
    );
    assert_left_alone_writes(&dry_at_two);
    assert_eq!(streak(&dry_at_two), Some((HEAD.to_owned(), 2)));
}

/// A failed disarm stops the transaction: the entry is not removed after it.
#[test]
fn a_disarm_failure_blocks_the_dequeue() {
    let outcome = run(&Fixture {
        armed_and_queued: true,
        disarm_fails: true,
        ..queued_unapproved()
    });
    assert!(outcome.calls.contains("disablePullRequestAutoMerge"));
    assert!(
        !outcome.calls.contains("dequeuePullRequest"),
        "{}",
        outcome.calls
    );
    assert!(
        outcome
            .report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("disarming because")),
        "{:?}",
        outcome.report
    );
}

/// An unaccepted dequeue (GraphQL `errors`, or no `dequeuePullRequest`) is an
/// error and is never recorded as a steward removal.
#[test]
fn an_unaccepted_dequeue_is_not_recorded() {
    for answer in [
        r#"{"data":{"dequeuePullRequest":null},"errors":[{"message":"not in queue"}]}"#,
        r#"{"data":{"dequeuePullRequest":null}}"#,
    ] {
        let outcome = run(&Fixture {
            dequeue_answer: Some(answer),
            ..queued_unapproved()
        });
        assert!(outcome.calls.contains("dequeuePullRequest"));
        assert!(
            outcome
                .report
                .error
                .as_deref()
                .is_some_and(|error| error.contains("dequeue was not accepted")),
            "{:?}",
            outcome.report
        );
        assert!(outcome.ledger.steward_removals.is_empty());
    }
}

/// No node ID, no mutation.
#[test]
fn an_empty_node_id_is_refused() {
    let outcome = run(&Fixture {
        empty_node_id: true,
        ..queued_unapproved()
    });
    assert!(
        !outcome.calls.contains("dequeuePullRequest"),
        "{}",
        outcome.calls
    );
    assert!(
        outcome
            .report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("node ID unavailable")),
        "{:?}",
        outcome.report
    );
}

/// The entry is judged on the head actually queued: an approval of the
/// census head does not keep an unapproved queued head, and an approval of
/// the queued head keeps it whatever the census saw.
#[test]
fn the_queued_head_is_judged_not_the_census_head() {
    let census_approved = run(&Fixture {
        live_head: MOVED,
        ..queued_approved()
    });
    assert_eq!(
        census_approved
            .report
            .mutation
            .as_deref()
            .and_then(|mutation| mutation.split(',').next()),
        Some(format!("dequeued_unapproved:{MOVED}").as_str()),
        "{:?}",
        census_approved.report
    );
    let queued_approved_head = run(&Fixture {
        live_head: MOVED,
        comments: approval_for(MOVED),
        ..queued_unapproved()
    });
    assert_left_alone(&queued_approved_head);
}

/// Labels the pipeline skips do not shield a queued head: an `owner-decision`,
/// `superseded` or `PARKED/<platform>` PR should never be in the queue, so on
/// positive evidence it is dequeued like any other.
#[test]
fn skipped_labels_do_not_shield_an_unapproved_queued_head() {
    for labels in [
        &["owner-decision"][..],
        &["superseded"][..],
        &["PARKED/linux"][..],
    ] {
        assert_dequeued(&run(&Fixture {
            labels,
            ..queued_unapproved()
        }));
    }
}

/// Report mode, the default, computes and reports the dequeue but never makes
/// it; apply mode makes it.
#[test]
fn report_mode_never_calls_dequeue_and_apply_mode_does() {
    let report = run(&Fixture {
        dequeue_mode: crate::app::merge_steward_cmd::DequeueMode::Report,
        ..queued_unapproved()
    });
    assert_eq!(
        report.report.mutation.as_deref(),
        Some(format!("would_dequeue_unapproved:{HEAD}").as_str()),
        "{:?}",
        report.report
    );
    assert!(
        !report.calls.contains("dequeuePullRequest"),
        "{}",
        report.calls
    );
    assert!(report.ledger.steward_removals.is_empty());
    assert_eq!(
        crate::app::merge_steward_cmd::DequeueMode::default(),
        crate::app::merge_steward_cmd::DequeueMode::Report
    );
    let apply = run(&queued_unapproved());
    assert_dequeued(&apply);
    // The disarm is not governed by the switch.
    assert_disarmed(&run(&Fixture {
        dequeue_mode: crate::app::merge_steward_cmd::DequeueMode::Report,
        ..Fixture::unapproved(Admission::NativeQueue)
    }));
}

#[test]
fn the_dequeue_mode_reads_the_machine_global_switch() {
    use crate::app::merge_steward_cmd::DequeueMode;
    let temp = tempfile::tempdir().expect("temp");
    assert_eq!(
        DequeueMode::from_machine_global(temp.path()),
        Ok(DequeueMode::Report)
    );
    for (value, mode) in [
        ("report", DequeueMode::Report),
        ("apply", DequeueMode::Apply),
    ] {
        fs::write(
            temp.path().join("config.toml"),
            format!("[merge_steward]\nunapproved_dequeue = \"{value}\"\n"),
        )
        .expect("config");
        assert_eq!(DequeueMode::from_machine_global(temp.path()), Ok(mode));
    }
    fs::write(
        temp.path().join("config.toml"),
        "[merge_steward]\nunapproved_dequeue = \"yes\"\n",
    )
    .expect("config");
    assert!(DequeueMode::from_machine_global(temp.path()).is_err());
}

/// A 404 that is not `gh`'s plain not-found for the config never reads as
/// absence: a token-mint or installation failure, or a 502 on a repository
/// whose name contains "404". Each defers instead of dequeuing.
#[test]
fn a_404_lookalike_is_not_absence() {
    for (shape, fixture) in [
        (
            "token mint 404",
            Fixture {
                policy_stderr: Some(
                    "ghapp: could not mint an installation token: gh: Not Found (HTTP 404)",
                ),
                ..queued_unapproved()
            },
        ),
        (
            "502 on a repo named 404",
            Fixture {
                repo: "owner/release-404",
                policy_unreadable: true,
                ..queued_unapproved()
            },
        ),
    ] {
        let outcome = run(&fixture);
        assert!(!outcome.calls.contains("dequeuePullRequest"), "{shape}");
        assert_eq!(
            outcome.report.mutation.as_deref(),
            Some(format!("dequeue_deferred_unreadable:{HEAD}").as_str()),
            "{shape}: {:?}",
            outcome.report
        );
    }
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
