//! The stale auto-merge sweep: an armed native auto-merge request on a head
//! the exact-head approval gate does not admit is disarmed on every `--apply`
//! pass, on the native-queue path and the server-enforced no-queue path,
//! whoever owns the pull request.
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
    let auto_merge = if armed {
        r#"{"enabledAt":"2026-10-28T22:42:00Z"}"#
    } else {
        "null"
    };
    format!(
        r#"{{"id":"PR_kw","number":42,"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":{auto_merge},"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}"#
    )
}

/// The census row: armed.
fn armed_pr() -> ObservedPr {
    let pr = parse_pr(
        &serde_json::from_str(&pr_row(true)).expect("PR json"),
        &BTreeMap::new(),
    )
    .expect("PR");
    assert!(pr.fact.auto_merge_active);
    pr
}

#[derive(Clone, Copy)]
enum Admission {
    NativeQueue,
    NoQueue,
}

fn policy_for(path: Admission) -> StewardPolicy {
    match path {
        Admission::NativeQueue => queue_policy(),
        Admission::NoQueue => StewardPolicy {
            merge_queue: false,
            required_checks: vec![
                RequiredCheck {
                    context: "macos".to_owned(),
                    app_id: None,
                },
                RequiredCheck {
                    context: "review-verdict".to_owned(),
                    app_id: Some(ACTIONS_APP),
                },
            ],
            ..queue_policy()
        },
    }
}

struct Fixture {
    path: Admission,
    /// Issue comments the approval gate reads; empty means no approval.
    comments: String,
    /// Whether the reviewer-policy read fails (approval unreadable).
    policy_unreadable: bool,
    /// Head the live queue-state read reports.
    live_head: &'static str,
    apply: bool,
}

impl Fixture {
    fn unapproved(path: Admission) -> Self {
        Self {
            path,
            comments: approval_for(OLD_HEAD),
            policy_unreadable: false,
            live_head: HEAD,
            apply: true,
        }
    }

    fn approved(path: Admission) -> Self {
        Self {
            comments: approval_for(HEAD),
            ..Self::unapproved(path)
        }
    }

    fn script(&self, log: &Path) -> String {
        let policy = if self.policy_unreadable {
            r#"echo "HTTP 502: Bad Gateway" >&2; exit 1"#.to_owned()
        } else {
            POLICY_REQUIRED.to_owned()
        };
        format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
printf '%s\n' "$*" >> '{log}'
case "$*" in
  *"contents/.shipyard/config.toml"*) {policy} ;;
  *"disablePullRequestAutoMerge"*)
    printf 'marker=%s\n' "${{SHIPYARD_INTERNAL_QUEUE_MUTATION:-}}" >> '{log}'
    # The real queue-removal guard judges the call, as `ghapp` would.
    python3 -I '{guard}' "$@" 2>> '{log}' || exit 97
    printf '%s' '{{"data":{{"disablePullRequestAutoMerge":{{"pullRequest":{{"number":42,"autoMergeRequest":null}}}}}}}}' ;;
  *"enablePullRequestAutoMerge"*) echo "the sweep must never arm" >&2; exit 92 ;;
  *"mergePullRequest("*) echo "the sweep must never merge" >&2; exit 93 ;;
  *"enqueuePullRequest"*) echo "the sweep must never enqueue" >&2; exit 91 ;;
  *"pulls/42/reviews"*) printf '' ;;
  *"repos/owner/repo/pulls/42 "*) printf 'feature\nmain\n' ;;
  *"query=query("*"mergeQueue(branch"*) printf '%s' '{merge_queue}' ;;
  *"issues/42/comments"*) printf '%s\n' '{comments}' ;;
  *"commits/{HEAD}"*) printf '%s' '2026-10-30T00:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":{{"enabledAt":"2026-10-28T22:42:00Z"}},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  "pr view "*) printf '%s' '{live_pr}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#,
            log = log.display(),
            guard = guard_script().display(),
            comments = self.comments,
            live_head = self.live_head,
            live_pr = pr_row(false),
            merge_queue = match self.path {
                Admission::NativeQueue =>
                    r#"{"data":{"repository":{"mergeQueue":{"entries":{"nodes":[],"pageInfo":{"hasNextPage":false,"endCursor":null}}}}}}"#,
                Admission::NoQueue => r#"{"data":{"repository":{"mergeQueue":null}}}"#,
            },
        )
    }
}

struct Outcome {
    report: PrReport,
    calls: String,
    ledger: StewardLedger,
}

fn run(fixture: &Fixture) -> Outcome {
    let temp = tempfile::tempdir().expect("temp");
    let log = temp.path().join("calls");
    let actions = fake_gh(&temp, &fixture.script(&log));
    let policy = policy_for(fixture.path);
    let mut observation =
        observation_for(armed_pr(), matches!(fixture.path, Admission::NativeQueue));
    observation.allow_auto_merge = true;
    observation.required_checks = policy.required_checks.clone();
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
        apply: fixture.apply,
        ledger: None,
    };
    let policy = StewardPolicy {
        managed_label: Some(MANAGED_LABEL.to_owned()),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        ..policy
    };
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let (mut reports, _failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &policy,
        &temp.path().join("ledger.json"),
        &mut ledger,
        fixture.apply.then_some(&control),
    );
    Outcome {
        report: reports.remove(0),
        calls: fs::read_to_string(log).unwrap_or_default(),
        ledger,
    }
}

fn assert_disarmed(outcome: &Outcome) {
    assert_eq!(
        outcome.report.mutation.as_deref(),
        Some(format!("disarmed_stale_auto_merge:{HEAD}").as_str()),
        "{:?}\n{}",
        outcome.report,
        outcome.calls
    );
    // The unmanaged PR's later label/recovery reads are not served by this
    // fixture; only the disarm's own outcome is under test here.
    assert!(
        !outcome
            .report
            .error
            .as_deref()
            .is_some_and(|error| error.contains("auto-merge") || error.contains("disarm")),
        "{:?}",
        outcome.report
    );
    // Shipyard's audited internal path, which the real removal guard accepted.
    assert!(outcome.calls.contains("marker=1"), "{}", outcome.calls);
    assert!(
        !outcome.calls.contains("refusing"),
        "the removal guard refused the disarm:\n{}",
        outcome.calls
    );
    assert!(
        outcome
            .ledger
            .audit
            .iter()
            .any(|entry| entry.action == "disarm_stale_auto_merge"
                && entry.subject == format!("pr:42:{HEAD}")),
        "no ledger audit for the disarm"
    );
}

fn assert_left_armed(outcome: &Outcome) {
    assert!(
        !outcome.calls.contains("disablePullRequestAutoMerge"),
        "{}",
        outcome.calls
    );
    assert!(
        !outcome
            .report
            .mutation
            .as_deref()
            .is_some_and(|mutation| mutation.contains("disarm")),
        "{:?}",
        outcome.report
    );
}

/// Replays #11117: an arm enabled for an older head survived a push by an
/// agent with write access, and the new head carries no approval.
#[test]
fn an_armed_unapproved_head_on_the_queue_path_is_disarmed() {
    let outcome = run(&Fixture::unapproved(Admission::NativeQueue));
    assert_eq!(outcome.report.decision, StewardDecision::Unmanaged);
    assert_disarmed(&outcome);
}

#[test]
fn an_armed_approved_head_on_the_queue_path_is_left_alone() {
    assert_left_armed(&run(&Fixture::approved(Admission::NativeQueue)));
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
    assert_left_armed(&run(&Fixture::approved(Admission::NoQueue)));
}

#[test]
fn an_unreadable_approval_disarms_on_the_no_queue_path() {
    assert_disarmed(&run(&Fixture {
        policy_unreadable: true,
        ..Fixture::unapproved(Admission::NoQueue)
    }));
}

#[test]
fn a_dry_run_lists_what_it_would_disarm_and_changes_nothing() {
    for path in [Admission::NativeQueue, Admission::NoQueue] {
        let outcome = run(&Fixture {
            apply: false,
            ..Fixture::unapproved(path)
        });
        assert_eq!(
            outcome.report.mutation.as_deref(),
            Some(format!("would_disarm_stale_auto_merge:{HEAD}").as_str()),
            "{:?}\n{}",
            outcome.report,
            outcome.calls
        );
        assert!(
            !outcome.calls.contains("disablePullRequestAutoMerge"),
            "{}",
            outcome.calls
        );
    }
}
