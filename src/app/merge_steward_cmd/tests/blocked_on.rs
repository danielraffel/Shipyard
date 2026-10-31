//! Every open pull request names why it is not merging, and since when.
use super::*;
use crate::app::merge_steward_cmd::blocked_on::{
    GateRead, RowFacts, SinceHint, Stall, classify, summarize,
};
use crate::head_approval::ApprovalBlocker;

const HEAD: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const CHECKS_GREEN_AT: &str = "2026-07-26T00:00:00Z";

/// A pull request with `labels`, `merge_state` and the given rollup.
fn pr_with(number: u64, labels: &[&str], merge_state: &str, rollup: &Value) -> ObservedPr {
    parse_pr(
        &json!({
            "id": format!("PR_{number}"),
            "number": number,
            "isDraft": false,
            "baseRefName": "main",
            "headRefOid": HEAD,
            "headRefName": "feature",
            "mergeStateStatus": merge_state,
            "autoMergeRequest": null,
            "author": {"login": "app/shipyard-local"},
            "labels": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>(),
            "statusCheckRollup": rollup,
        }),
        &BTreeMap::new(),
    )
    .expect("PR")
}

fn green_rollup() -> Value {
    json!([{"__typename": "CheckRun", "name": "macos", "status": "COMPLETED",
            "conclusion": "SUCCESS", "completedAt": CHECKS_GREEN_AT,
            "detailsUrl": "https://github.com/owner/repo/actions/runs/100"}])
}

fn row(pr: &ObservedPr, mutation: Option<&str>, approval: GateRead<'_>) -> (Stall, SinceHint) {
    let policy = queue_policy();
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation,
            error: None,
            approval,
            enqueued_at: None,
        },
        &policy,
    )
}

fn token(pr: &ObservedPr, mutation: Option<&str>) -> String {
    row(pr, mutation, GateRead::Unread).0.blocked_on
}

#[test]
fn a_green_head_nobody_reviewed_is_no_verdict_since_its_checks_went_green() {
    let pr = pr_with(1, &[], "CLEAN", &green_rollup());
    let (blocked, hint) = row(
        &pr,
        Some("waiting_head_approval"),
        GateRead::Refused(&ApprovalBlocker::NoVerdict),
    );
    assert_eq!(blocked.blocked_on, "no_verdict");
    assert_eq!(blocked.since.as_deref(), Some(CHECKS_GREEN_AT));
    assert_eq!(blocked.since_source, Some("checks_green"));
    assert_eq!(hint, SinceHint::Known);
}

#[test]
fn a_held_head_names_holder_and_comment() {
    let pr = pr_with(2, &[], "CLEAN", &green_rollup());
    let hold = ApprovalBlocker::Held {
        holder: "infra-review2".to_owned(),
        record: "comment",
        id: Some(6_544_000_001),
        at: Some("2026-10-31T15:00:00Z".to_owned()),
    };
    let (blocked, _) = row(&pr, Some("waiting_head_approval"), GateRead::Refused(&hold));
    assert_eq!(blocked.blocked_on, "held");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T15:00:00Z"));
    assert_eq!(blocked.since_source, Some("hold_posted"));
    let held_by = blocked.held_by.expect("holder");
    assert_eq!(
        (held_by.holder.as_str(), held_by.record, held_by.id),
        ("infra-review2", "comment", Some(6_544_000_001))
    );
}

/// The #11175 shape at the report layer: a refused head whose approval was
/// excluded reads as the exclusion, dated by the approval, never `no_verdict`.
#[test]
fn an_excluded_approval_never_reads_as_no_verdict() {
    let pr = pr_with(11175, &[], "CLEAN", &green_rollup());
    let excluded = ApprovalBlocker::Excluded {
        reason: "unattributed",
        detail: "its session stamp is unattributed".to_owned(),
        id: Some(6_544_869_514),
        at: Some("2026-10-31T14:43:31Z".to_owned()),
    };
    let (blocked, _) = row(
        &pr,
        Some("waiting_head_approval"),
        GateRead::Refused(&excluded),
    );
    assert_eq!(blocked.blocked_on, "approval_excluded:unattributed");
    assert_ne!(blocked.blocked_on, "no_verdict");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T14:43:31Z"));
    assert!(
        blocked
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("6544869514")),
        "{blocked:?}"
    );
}

#[test]
fn a_stale_head_approval_is_dated_by_this_heads_green_checks() {
    let pr = pr_with(3, &[], "CLEAN", &green_rollup());
    let stale = ApprovalBlocker::StaleHead {
        approved_head: "b".repeat(40),
        at: Some("2026-01-01T00:00:00Z".to_owned()),
    };
    let (blocked, _) = row(
        &pr,
        Some("waiting_head_approval"),
        GateRead::Refused(&stale),
    );
    assert_eq!(blocked.blocked_on, "approval_excluded:stale_head");
    assert_eq!(blocked.since.as_deref(), Some(CHECKS_GREEN_AT));
}

#[test]
fn a_refusal_whose_gate_could_not_be_read_says_so() {
    let pr = pr_with(4, &[], "CLEAN", &green_rollup());
    assert_eq!(token(&pr, Some("waiting_head_approval")), "approval_unread");
}

#[test]
fn owner_decision_and_skip_labels_win_over_the_pipeline_state() {
    let owner = pr_with(5, &["owner-decision"], "CLEAN", &green_rollup());
    let (blocked, hint) = row(&owner, Some("waiting_head_approval"), GateRead::Unread);
    assert_eq!(blocked.blocked_on, "owner_decision");
    assert_eq!(hint, SinceHint::Labeled("owner-decision".to_owned()));
    let superseded = pr_with(6, &["superseded"], "DIRTY", &green_rollup());
    assert_eq!(token(&superseded, None), "excluded_label:superseded");
    let parked = pr_with(7, &["PARKED/linux"], "CLEAN", &green_rollup());
    assert_eq!(token(&parked, None), "excluded_label:PARKED/linux");
    let opted_out = pr_with(8, &["steward:skip"], "CLEAN", &green_rollup());
    assert_eq!(token(&opted_out, None), "excluded_label:steward:skip");
}

#[test]
fn merge_state_draft_and_queue_are_named() {
    let dirty = pr_with(9, &[], "DIRTY", &green_rollup());
    let (blocked, hint) = row(&dirty, None, GateRead::Unread);
    assert_eq!(blocked.blocked_on, "merge_state:DIRTY");
    assert_eq!(hint, SinceHint::HeadPushed);

    let mut draft = pr_with(10, &[], "CLEAN", &green_rollup());
    draft.fact.draft = true;
    assert_eq!(token(&draft, None), "draft");

    let mut queued = pr_with(11, &[], "CLEAN", &green_rollup());
    queued.fact.queue_position = Some(2);
    let policy = queue_policy();
    let decision = classify_pr(&queued.fact, &policy, &BTreeMap::new());
    let (blocked, _) = classify(
        &RowFacts {
            pr: &queued.fact,
            decision: &decision,
            mutation: None,
            error: None,
            approval: GateRead::Unread,
            enqueued_at: Some("2026-10-31T16:00:00Z"),
        },
        &policy,
    );
    assert_eq!(blocked.blocked_on, "queued");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T16:00:00Z"));
    assert_eq!(blocked.since_source, Some("merge_queue_enqueued"));
}

/// danielraffel/Shipyard#1082: a head whose required context never ran
/// under its current name (only the old `[local]` name ran) is reported as
/// `required_never_ran`, naming the context, not left out or called pending.
#[test]
fn shipyard_1082_a_required_check_that_never_ran_is_named() {
    let rollup = json!([{"__typename": "CheckRun", "name": "macos [local]",
        "status": "COMPLETED", "conclusion": "SUCCESS", "completedAt": CHECKS_GREEN_AT}]);
    let pr = pr_with(973, &[], "CLEAN", &rollup);
    let (blocked, hint) = row(&pr, None, GateRead::Unread);
    assert_eq!(blocked.blocked_on, "required_never_ran:macos");
    assert_eq!(hint, SinceHint::HeadPushed);
}

#[test]
fn pending_and_failed_required_checks_are_told_apart_and_dated() {
    let pending = pr_with(
        12,
        &[],
        "CLEAN",
        &json!([{"__typename": "CheckRun", "name": "macos", "status": "IN_PROGRESS",
                "startedAt": "2026-10-31T17:00:00Z"}]),
    );
    let (blocked, _) = row(&pending, None, GateRead::Unread);
    assert_eq!(blocked.blocked_on, "required_pending");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T17:00:00Z"));
    assert_eq!(blocked.since_source, Some("check_started"));

    let failed = pr_with(
        13,
        &[],
        "CLEAN",
        &json!([{"__typename": "CheckRun", "name": "macos", "status": "COMPLETED",
                "conclusion": "FAILURE", "completedAt": "2026-10-31T18:00:00Z"}]),
    );
    let (blocked, _) = row(&failed, None, GateRead::Unread);
    assert_eq!(blocked.blocked_on, "required_failed:macos");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T18:00:00Z"));
}

#[test]
fn admission_outcomes_are_named() {
    let pr = pr_with(14, &[], "CLEAN", &green_rollup());
    assert_eq!(
        token(&pr, Some("skipped_ejected_same_head:failed_checks")),
        "ejected_same_head:failed_checks"
    );
    assert_eq!(token(&pr, Some("enqueued")), "admitted");
    assert_eq!(
        token(&pr, Some(&format!("merged_exact_head:{HEAD}"))),
        "admitted"
    );
    assert_eq!(
        token(&pr, Some("waiting_full_test_tier")),
        "waiting_full_test_tier"
    );
    assert_eq!(
        token(&pr, Some("skipped_after_live_revalidation")),
        "revalidating"
    );
    let (ready, _) = row(&pr, None, GateRead::Admits);
    assert_eq!(ready.blocked_on, "ready");
}

#[test]
fn the_summary_counts_tokens_and_lists_the_three_oldest_blocked() {
    let report = |number: u64, token: &str, since: Option<&str>| PrReport {
        number,
        head_sha: HEAD.to_owned(),
        decision: StewardDecision::ArmMergeQueue,
        mutation: None,
        error: None,
        blocked: Stall {
            blocked_on: token.to_owned(),
            since: since.map(str::to_owned),
            ..Stall::default()
        },
    };
    let reports = [
        report(1, "no_verdict", Some("2026-10-31T10:00:00Z")),
        report(2, "no_verdict", Some("2026-10-31T11:00:00Z")),
        report(3, "held", Some("2026-10-31T08:00:00Z")),
        report(4, "queued", Some("2026-10-30T00:00:00Z")),
        report(
            5,
            "approval_excluded:unattributed",
            Some("2026-10-31T09:00:00Z"),
        ),
        report(6, "merge_state:DIRTY", None),
    ];
    let now = DateTime::parse_from_rfc3339("2026-10-31T12:00:00Z")
        .expect("now")
        .with_timezone(&Utc);
    let summary = summarize(&reports, now);
    assert_eq!(summary.counts.get("no_verdict"), Some(&2));
    assert_eq!(summary.counts.get("queued"), Some(&1));
    assert_eq!(summary.counts.values().sum::<usize>(), reports.len());
    let oldest: Vec<(u64, &str, i64)> = summary
        .oldest
        .iter()
        .map(|entry| (entry.number, entry.blocked_on.as_str(), entry.age_minutes))
        .collect();
    assert_eq!(
        oldest,
        vec![
            (3, "held", 240),
            (5, "approval_excluded:unattributed", 180),
            (1, "no_verdict", 120),
        ],
        "queued is not blocked; an undated row is counted but not aged"
    );
}

/// The apply pass's admission attempt reads the gate once; the report reuses
/// that read instead of asking GitHub again, and names the exclusion.
#[cfg(unix)]
#[test]
fn the_admission_gate_read_carries_its_diagnosis_into_the_report() {
    let temp = tempfile::tempdir().expect("temp");
    let comment = r#"{"id":6544869514,"login":"shipyard-local[bot]","type":"Bot","association":"CONTRIBUTOR","at":"2026-10-31T14:43:31Z","body":"reviewer: pulp-review\n<!-- session: Daniels-Mac-Studio-m3/unattributed agent: pulp-review -->\nverdict: APPROVE head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'reviewer_signers = ["pulp-review"]' ;;
  *"pulls/42/reviews"*) ;;
  *"issues/42/comments"*) printf '%s\n' '{comment}' ;;
  *"commits/{HEAD}"*"--jq"*) printf '%s\n' '2026-10-31T14:00:00Z' ;;
  *"pulls/42"*"--jq"*) printf '%s\n' 'shipyard-local[bot]' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":42,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
        ),
    );
    let pr = ready_pr();
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "Generous-Corp/pulp".to_owned();
    let control = mutation_control(&temp, "studio", "studio");
    let ledger_path = temp.path().join("ledger.json");
    let context = mutation_apply_context(&actions, &observation, &ledger_path, &control);
    let ledger = StewardLedger::default();
    let (gate, _, _) = crate::head_approval::with_host_review_default(true, || {
        crate::app::merge_steward_cmd::pr_mutations::read_head_gate_with_queue_state(
            &context, &pr, &ledger,
        )
    })
    .expect("gate read");
    assert!(!gate.allows(), "{gate:?}");
    let blockers = context.approval_blockers.borrow();
    let (head, diagnosis) = blockers.get(&42).expect("the read was recorded");
    assert_eq!(head, HEAD);
    let diagnosis = diagnosis.as_ref().expect("a refusal is diagnosed");
    let policy = queue_policy();
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let explain = gate.explain();
    let (blocked, _) = classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation: Some("waiting_head_approval"),
            error: Some(explain.as_str()),
            approval: GateRead::Refused(diagnosis),
            enqueued_at: None,
        },
        &policy,
    );
    assert_eq!(blocked.blocked_on, "approval_excluded:unattributed");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T14:43:31Z"));
}

/// No open pull request is left out of the pass, and each carries a token,
/// whatever its state. Dating reads that fail leave `since` null.
#[cfg(unix)]
#[test]
fn every_open_pull_request_in_a_pass_carries_a_token() {
    let temp = tempfile::tempdir().expect("temp");
    let actions = fake_gh(
        &temp,
        r#"
case "$*" in
  *"commits/"*) printf '%s' '{"commit":{"committer":{"date":"2026-10-31T01:00:00Z"}}}' ;;
  *"issues/5/events"*) printf 'owner-decision\t2026-10-31T05:00:00Z\n' ;;
  *) echo "unreadable" >&2; exit 2 ;;
esac
"#,
    );
    let never_ran = json!([{"__typename": "CheckRun", "name": "macos [local]",
        "status": "COMPLETED", "conclusion": "SUCCESS", "completedAt": CHECKS_GREEN_AT}]);
    let mut draft = pr_with(4, &[], "CLEAN", &green_rollup());
    draft.fact.draft = true;
    let mut unmanaged = pr_with(6, &[], "CLEAN", &green_rollup());
    unmanaged.fact.author = Some("someone".to_owned());
    let prs = vec![
        pr_with(1, &[], "CLEAN", &never_ran),
        pr_with(2, &[], "DIRTY", &green_rollup()),
        pr_with(3, &[], "CLEAN", &green_rollup()),
        draft,
        pr_with(5, &["owner-decision"], "CLEAN", &green_rollup()),
        unmanaged,
    ];
    let mut observation = observation_for(prs[0].clone(), true);
    observation.prs = prs;
    let args = StewardCommandArgs {
        repos: vec!["owner/repo".to_owned()],
        base: "main".to_owned(),
        opt_out_label: "steward:skip".to_owned(),
        provenance_blocking_labels: default_provenance_blocking_labels(),
        managed_label: MANAGED_LABEL.to_owned(),
        handoff_context: HANDOFF_CONTEXT.to_owned(),
        max_transient_reruns: 1,
        recover_hosted_setup_eviction_priority: false,
        coalesce: false,
        preempt_capacity: false,
        max_preemptions_per_head: 1,
        arm_unqueued: false,
        approval_context: crate::merge_steward::DEFAULT_APPROVAL_CONTEXT.to_owned(),
        wake_owner: false,
        unapproved_dequeue: crate::app::merge_steward_cmd::DequeueMode::Report,
        apply: false,
        ledger: None,
    };
    let mut ledger = StewardLedger::default();
    let (report, _, _) = apply_repo_plan(
        &actions,
        &args,
        &observation,
        &temp.path().join("ledger.json"),
        &mut ledger,
        0,
        None,
    );
    let tokens: Vec<(u64, &str, Option<&str>)> = report
        .prs
        .iter()
        .map(|pr| {
            (
                pr.number,
                pr.blocked.blocked_on.as_str(),
                pr.blocked.since.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        tokens,
        vec![
            (1, "required_never_ran:macos", Some("2026-10-31T01:00:00Z")),
            (2, "merge_state:DIRTY", Some("2026-10-31T01:00:00Z")),
            (3, "approval_unread", Some(CHECKS_GREEN_AT)),
            (4, "draft", Some("2026-10-31T01:00:00Z")),
            (5, "owner_decision", Some("2026-10-31T05:00:00Z")),
            (6, "unmanaged", Some("2026-10-31T01:00:00Z")),
        ]
    );
    assert_eq!(
        report.blocked_summary.counts.values().sum::<usize>(),
        report.prs.len()
    );
    let rendered = serde_json::to_value(&report).expect("json");
    let first = &rendered["prs"][0];
    assert_eq!(first["blocked_on"], json!("required_never_ran:macos"));
    assert_eq!(first["since"], json!("2026-10-31T01:00:00Z"));
    assert_eq!(first["since_source"], json!("head_commit"));
    assert!(rendered["blocked_summary"]["oldest"].is_array());
}
