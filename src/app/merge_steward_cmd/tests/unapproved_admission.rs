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
        r#"{{"id":"PR_kw{number}","number":{number},"state":"OPEN","isDraft":false,"baseRefName":"main","headRefOid":"{HEAD}","headRefName":"feature","mergeStateStatus":"BLOCKED","autoMergeRequest":{auto_merge},"labels":[],"statusCheckRollup":[{{"__typename":"CheckRun","name":"macos","status":"COMPLETED","conclusion":"SUCCESS","completedAt":"2026-10-30T00:00:00Z","detailsUrl":"https://github.com/owner/repo/actions/runs/100"}}]}}"#
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
        let policy = if self.policy_unreadable {
            r#"echo "HTTP 502: Bad Gateway" >&2; exit 1"#.to_owned()
        } else {
            POLICY_REQUIRED.to_owned()
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
  *"/reviews"*) printf '' ;;
  *"repos/owner/repo/pulls/4"[0-9]" "*) printf 'feature\nmain\n' ;;
  *"query=query("*"mergeQueue(branch"*) printf '%s' '{merge_queue}' ;;
  *"/comments"*) printf '%s\n' '{comments}' ;;
  *"commits/{HEAD}"*) printf '%s' '2026-10-30T00:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{live_head}","isInMergeQueue":{in_queue},"mergeQueueEntry":{entry},"autoMergeRequest":{auto_merge},"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
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
                Admission::NoQueue | Admission::Refused =>
                    r#"{"data":{"repository":{"mergeQueue":null}}}"#,
            },
        )
    }
}

struct Outcome {
    reports: Vec<PrReport>,
    report: PrReport,
    calls: String,
    ledger: StewardLedger,
}

fn run(fixture: &Fixture) -> Outcome {
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
    let mut ledger = StewardLedger::default();
    let control = mutation_control(&temp, "studio", "studio");
    let (reports, _failed) = apply_pr_plans(
        &actions,
        &args,
        &observation,
        &policy,
        &temp.path().join("ledger.json"),
        &mut ledger,
        fixture.apply.then_some(&control),
    );
    Outcome {
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
    assert_eq!(
        outcome.report.mutation.as_deref(),
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

#[test]
fn an_unreadable_approval_dequeues() {
    assert_dequeued(&run(&Fixture {
        queued: true,
        policy_unreadable: true,
        ..Fixture::unapproved(Admission::NativeQueue)
    }));
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
