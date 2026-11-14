//! Every open pull request names why it is not merging, and since when.
use super::*;
use crate::app::merge_steward_cmd::blocked_on::{
    self, GateRead, RowFacts, SinceHint, Stall, classify, summarize,
};
use crate::head_approval::ApprovalBlocker;
use crate::merge_steward::StewardCheckSource;

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
            ejection: None,
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
    assert_eq!(blocked.blocked_on, "held:review-verdict");
    assert_eq!(blocked.since.as_deref(), Some("2026-10-31T15:00:00Z"));
    assert_eq!(blocked.since_source, Some("hold_posted"));
    assert_eq!(
        blocked.blocked_detail.as_deref(),
        Some("held head: needs a new head")
    );
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
            ejection: None,
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

/// 2026-11-12: an agent read "required macOS job queued, all slots busy" as an
/// external blocker and stopped four times. A required check run that is only
/// queued is `capacity_queued`, with a next action that says it is not blocked.
#[test]
fn a_queued_required_job_is_capacity_queued_and_not_blocked() {
    let queued = pr_with(
        14,
        &[],
        "CLEAN",
        &json!([{"__typename": "CheckRun", "name": "macos", "status": "QUEUED",
                "startedAt": "2026-11-12T17:00:00Z"}]),
    );
    let (blocked, _) = row(&queued, None, GateRead::Unread);
    assert_eq!(blocked.blocked_on, "capacity_queued");
    assert_eq!(blocked.since.as_deref(), Some("2026-11-12T17:00:00Z"));
    // Control: the same required job once a runner has started it.
    let running = pr_with(
        15,
        &[],
        "CLEAN",
        &json!([{"__typename": "CheckRun", "name": "macos", "status": "IN_PROGRESS",
                "startedAt": "2026-11-12T17:00:00Z"}]),
    );
    assert_eq!(token(&running, None), "required_pending");
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
    // A definitive GitHub refusal names itself, never `steward_error`.
    assert_eq!(token(&pr, Some("code_owner_review")), "code_owner_review");
    assert_eq!(
        token(&pr, Some("enqueue_refused:Base branch was modified")),
        "enqueue_refused:Base branch was modified"
    );
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
    let record = blockers.get(&42).expect("the read was recorded");
    assert_eq!(record.head, HEAD);
    assert_eq!(record.ejection, None);
    let diagnosis = record.blocker.as_ref().expect("a refusal is diagnosed");
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
            ejection: None,
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
        infra_rerun: crate::app::merge_steward_cmd::InfraRerunMode::Off,
        pass_budget: None,
        pass_deadline: None,
        invariants: None,
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

/// The real `RemovedFromMergeQueueEvent` timeline of PR 8811: the queue
/// removed head `e147f2d0` for `failed_checks` and no new head followed.
const HEAD_8811: &str = "e147f2d09972babcc9977e82a46470e17f9de538";

fn queue_8811() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/github/pr_real_8811_same_head_ejected.json");
    serde_json::from_str(&fs::read_to_string(path).expect("fixture")).expect("json")
}

/// Generous-Corp/pulp#11308 read `ready` in a dry run while the apply tick
/// skipped it as `skipped_ejected_same_head:failed_checks`. Both now read the
/// ejection from the same queue-state answer, dated by the removal.
#[test]
fn an_ejected_head_reads_the_same_in_a_dry_run_and_an_apply_pass() {
    let mut pr = pr_with(11308, &[], "CLEAN", &green_rollup());
    pr.fact.head_sha = HEAD_8811.to_owned();
    let record = blocked_on::GateRecord::new(HEAD_8811, None, &queue_8811(), false);
    let ejection = record.ejection.as_ref().expect("same-head ejection");
    assert_eq!(ejection.reason, "failed_checks");
    let policy = queue_policy();
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let facts = |mutation: Option<&'static str>| RowFacts {
        pr: &pr.fact,
        decision: &decision,
        mutation,
        error: None,
        approval: GateRead::Admits,
        enqueued_at: None,
        ejection: Some(ejection),
    };
    let (dry_run, _) = classify(&facts(None), &policy);
    let (apply, _) = classify(
        &facts(Some("skipped_ejected_same_head:failed_checks")),
        &policy,
    );
    assert_eq!(dry_run, apply);
    assert_eq!(dry_run.blocked_on, "ejected_same_head:failed_checks");
    assert_eq!(dry_run.since.as_deref(), Some("2026-09-25T04:12:51Z"));
    assert_eq!(dry_run.since_source, Some("queue_removed"));
    // An unapproved ejected head: the apply path stops at the approval, but
    // the queue's refusal still names it, so both modes agree.
    let unapproved = ApprovalBlocker::NoVerdict;
    let (refused, _) = classify(
        &RowFacts {
            mutation: Some("waiting_head_approval"),
            approval: GateRead::Refused(&unapproved),
            ..facts(None)
        },
        &policy,
    );
    assert_eq!(refused.blocked_on, "ejected_same_head:failed_checks");
    // Control: the same queue answer for a different head is no ejection.
    let other = blocked_on::GateRecord::new(HEAD, None, &queue_8811(), false);
    assert_eq!(other.ejection, None);
}

/// The retry classifier's reason rides on the ejection token, so a repair
/// lane reads the failing job and whether the retry is spent; a head it
/// clears for its one retry is an ordinary admission candidate instead.
#[test]
fn an_ejection_carries_the_retry_verdict_as_its_detail() {
    let mut pr = pr_with(11308, &[], "CLEAN", &green_rollup());
    pr.fact.head_sha = HEAD_8811.to_owned();
    let record = blocked_on::GateRecord::new(HEAD_8811, None, &queue_8811(), false);
    let mut ejection = record.ejection.clone().expect("same-head ejection");
    let policy = queue_policy();
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let classify_with = |ejection: &blocked_on::SameHeadEjection| {
        classify(
            &RowFacts {
                pr: &pr.fact,
                decision: &decision,
                mutation: None,
                error: None,
                approval: GateRead::Admits,
                enqueued_at: None,
                ejection: Some(ejection),
            },
            &policy,
        )
        .0
    };
    ejection.retry = Some(blocked_on::SameHeadRetry {
        allowed: false,
        detail: "required check `macos` concluded `failure`; the one not-head-caused retry \
                 this head is allowed has been spent"
            .to_owned(),
    });
    let spent = classify_with(&ejection);
    assert_eq!(spent.blocked_on, "ejected_same_head:failed_checks");
    assert!(
        spent
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("`macos`") && detail.contains("spent")),
        "{spent:?}"
    );
    ejection.retry = Some(blocked_on::SameHeadRetry {
        allowed: true,
        detail: "passed on head".to_owned(),
    });
    assert_eq!(classify_with(&ejection).blocked_on, "ready");
    // Control: no verdict keeps the bare ejection token.
    ejection.retry = None;
    let bare = classify_with(&ejection);
    assert_eq!(bare.blocked_on, "ejected_same_head:failed_checks");
    assert_eq!(bare.blocked_detail, None);
}

fn app_bound_policy(contexts: &[&str]) -> StewardPolicy {
    let mut policy = queue_policy();
    policy.required_checks = contexts
        .iter()
        .map(|context| RequiredCheck {
            context: (*context).to_owned(),
            app_id: Some(15368),
        })
        .collect();
    policy
}

fn app_check(name: &str, status: &str, conclusion: Option<&str>, at: &str) -> Value {
    json!({"__typename": "CheckRun", "name": name, "status": status, "conclusion": conclusion,
           "startedAt": at, "completedAt": if status == "COMPLETED" { Some(at) } else { None },
           "checkSuite": {"app": {"databaseId": 15368}}})
}

fn review_status(description: Option<&str>) -> StewardCheck {
    StewardCheck {
        name: "review-verdict".to_owned(),
        source: StewardCheckSource::StatusContext,
        app_id: Some(15368),
        status: "COMPLETED".to_owned(),
        conclusion: Some("FAILURE".to_owned()),
        run_id: None,
        observed_at: Some("2026-11-11T11:25:01Z".to_owned()),
        check_id: Some(7),
        description: description.map(str::to_owned),
    }
}

/// Generous-Corp/vellum#126: `sterile-consumer` `needs:` `gpu-macos-arm64`,
/// which is still running, so the context has not appeared yet. That is
/// pending; only an absent context with nothing in flight never ran.
#[test]
fn an_absent_context_behind_a_running_job_is_pending_not_never_ran() {
    let policy = app_bound_policy(&["gpu-macos-arm64", "sterile-consumer"]);
    let running = pr_with(
        126,
        &[],
        "CLEAN",
        &json!([app_check(
            "gpu-macos-arm64",
            "IN_PROGRESS",
            None,
            "2026-10-31T19:05:00Z"
        )]),
    );
    let classify_row = |pr: &ObservedPr| {
        let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
        classify(
            &RowFacts {
                pr: &pr.fact,
                decision: &decision,
                mutation: None,
                error: None,
                approval: GateRead::Unread,
                enqueued_at: None,
                ejection: None,
            },
            &policy,
        )
        .0
    };
    let stall = classify_row(&running);
    assert_eq!(stall.blocked_on, "required_pending");
    assert_eq!(stall.since.as_deref(), Some("2026-10-31T19:05:00Z"));
    // Control: the producer finished and the context still never appeared.
    let finished = pr_with(
        126,
        &[],
        "CLEAN",
        &json!([app_check(
            "gpu-macos-arm64",
            "COMPLETED",
            Some("SUCCESS"),
            "2026-10-31T19:30:00Z"
        )]),
    );
    assert_eq!(
        classify_row(&finished).blocked_on,
        "required_never_ran:sterile-consumer"
    );
}

/// A review-verdict job that is still running is pending, not failed: with
/// no gate read the check token (`required_pending`) must stand instead of a
/// synthesized `required_failed:review-verdict`.
#[test]
fn a_running_review_verdict_stays_pending_without_a_gate_read() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let pr = pr_with(
        1092,
        &[],
        "CLEAN",
        &json!([
            app_check(
                "macos",
                "COMPLETED",
                Some("SUCCESS"),
                "2026-10-31T18:00:00Z"
            ),
            app_check(
                "review-verdict",
                "IN_PROGRESS",
                None,
                "2026-10-31T18:40:16Z"
            ),
        ]),
    );
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    assert!(matches!(decision, StewardDecision::WaitingRequired { .. }));
    let facts = RowFacts {
        pr: &pr.fact,
        decision: &decision,
        mutation: None,
        error: None,
        approval: GateRead::Unread,
        enqueued_at: None,
        ejection: None,
    };
    let (stall, _) = classify(&facts, &policy);
    assert_eq!(stall.blocked_on, "required_pending");
}

/// The approval job fails its check run while a head only awaits a verdict,
/// after posting a `pending` status. That is waiting on the reviewer: not
/// `required_failed`, no needs-agent recovery, and the author has nothing to do.
#[test]
fn a_pending_review_verdict_under_a_failed_check_run_waits_on_the_reviewer() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let mut pr = pr_with(
        1100,
        &[],
        "CLEAN",
        &json!([
            app_check(
                "macos",
                "COMPLETED",
                Some("SUCCESS"),
                "2026-11-03T09:00:00Z"
            ),
            app_check(
                "review-verdict",
                "COMPLETED",
                Some("FAILURE"),
                "2026-11-03T10:00:05Z"
            ),
        ]),
    );
    pr.fact.checks.push(crate::merge_steward::StewardCheck {
        name: "review-verdict".to_owned(),
        source: crate::merge_steward::StewardCheckSource::StatusContext,
        app_id: Some(15368),
        status: "IN_PROGRESS".to_owned(),
        conclusion: None,
        run_id: None,
        observed_at: Some("2026-11-03T10:00:00Z".to_owned()),
        check_id: Some(5),
        description: None,
    });
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    assert!(
        matches!(decision, StewardDecision::WaitingRequired { .. }),
        "{decision:?}"
    );
    let facts = RowFacts {
        pr: &pr.fact,
        decision: &decision,
        mutation: None,
        error: None,
        approval: GateRead::Unread,
        enqueued_at: None,
        ejection: None,
    };
    let (stall, _) = classify(&facts, &policy);
    assert_eq!(stall.blocked_on, "required_pending");
    let next = crate::app::merge_steward_cmd::next_action::for_row(
        &crate::app::merge_steward_cmd::next_action::RowContext {
            repo: "o/r",
            base: "main",
            pr: &pr.fact,
            policy: &policy,
            error: None,
        },
        &stall,
    );
    assert!(!next.actionable(), "{next:?}");
    assert!(
        next.action.contains("nothing for the author to do"),
        "{next:?}"
    );
    // Control: the same failed check run with a `failure` status is a real
    // failure, named as the approval failing.
    let pr_failed = {
        let mut failed = pr.clone();
        failed.fact.checks.last_mut().expect("status").status = "COMPLETED".to_owned();
        failed.fact.checks.last_mut().expect("status").conclusion = Some("FAILURE".to_owned());
        failed
    };
    let failed = classify_pr(&pr_failed.fact, &policy, &BTreeMap::new());
    assert!(
        matches!(failed, StewardDecision::RequiredFailed { .. }),
        "{failed:?}"
    );
}

/// A failed ordinary check stays actionable when the approval status is
/// pending: the failure is named and recovery still sees `RequiredFailed`.
#[test]
fn a_failed_macos_check_is_not_hidden_by_a_pending_review_verdict() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let mut pr = pr_with(
        1101,
        &[],
        "CLEAN",
        &json!([
            app_check(
                "macos",
                "COMPLETED",
                Some("FAILURE"),
                "2026-11-03T09:00:00Z"
            ),
            app_check(
                "review-verdict",
                "COMPLETED",
                Some("FAILURE"),
                "2026-11-03T10:00:05Z"
            ),
        ]),
    );
    pr.fact.checks.push(crate::merge_steward::StewardCheck {
        name: "review-verdict".to_owned(),
        source: crate::merge_steward::StewardCheckSource::StatusContext,
        app_id: Some(15368),
        status: "IN_PROGRESS".to_owned(),
        conclusion: None,
        run_id: None,
        observed_at: Some("2026-11-03T10:00:00Z".to_owned()),
        check_id: Some(5),
        description: None,
    });
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let StewardDecision::RequiredFailed { contexts } = &decision else {
        panic!("expected RequiredFailed, got {decision:?}");
    };
    assert!(
        contexts.iter().all(|c| c.starts_with("macos")),
        "{contexts:?}"
    );
    let facts = RowFacts {
        pr: &pr.fact,
        decision: &decision,
        mutation: None,
        error: None,
        approval: GateRead::Unread,
        enqueued_at: None,
        ejection: None,
    };
    let (stall, _) = classify(&facts, &policy);
    assert!(
        stall.blocked_on.starts_with("required_failed:macos"),
        "{}",
        stall.blocked_on
    );
}

/// danielraffel/Shipyard#969 read `required_failed:review-verdict` while
/// the real cause was its APPROVE's signer, which the base does not count.
/// When the approval context is the only red required context, the approval
/// diagnosis is the token; without a gate read the check token stays and
/// says why.
#[test]
fn a_red_review_verdict_names_the_approval_not_the_check() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let pr = pr_with(
        969,
        &[],
        "CLEAN",
        &json!([
            app_check(
                "macos",
                "COMPLETED",
                Some("SUCCESS"),
                "2026-10-31T18:00:00Z"
            ),
            app_check(
                "review-verdict",
                "COMPLETED",
                Some("FAILURE"),
                "2026-10-31T18:40:16Z"
            ),
        ]),
    );
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    assert!(matches!(decision, StewardDecision::RequiredFailed { .. }));
    let uncounted = ApprovalBlocker::Excluded {
        reason: "uncounted_signer",
        detail: "its signer pulp-review2 is not one of infra-review2".to_owned(),
        id: Some(6_550_000_000),
        at: Some("2026-10-31T18:30:00Z".to_owned()),
    };
    let facts = |approval| RowFacts {
        pr: &pr.fact,
        decision: &decision,
        mutation: None,
        error: None,
        approval,
        enqueued_at: None,
        ejection: None,
    };
    let (named, _) = classify(&facts(GateRead::Refused(&uncounted)), &policy);
    assert_eq!(named.blocked_on, "approval_excluded:uncounted_signer");
    assert_eq!(named.since.as_deref(), Some("2026-10-31T18:30:00Z"));
    let (unread, _) = classify(&facts(GateRead::Unread), &policy);
    assert_eq!(unread.blocked_on, "required_failed:review-verdict");
    assert!(unread.blocked_detail.is_some());
    // Control: another red context besides the approval keeps the check token.
    let both = pr_with(
        969,
        &[],
        "CLEAN",
        &json!([
            app_check(
                "macos",
                "COMPLETED",
                Some("FAILURE"),
                "2026-10-31T18:00:00Z"
            ),
            app_check(
                "review-verdict",
                "COMPLETED",
                Some("FAILURE"),
                "2026-10-31T18:40:16Z"
            ),
        ]),
    );
    let decision = classify_pr(&both.fact, &policy, &BTreeMap::new());
    let (red, _) = classify(
        &RowFacts {
            pr: &both.fact,
            decision: &decision,
            ..facts(GateRead::Refused(&uncounted))
        },
        &policy,
    );
    assert_eq!(red.blocked_on, "required_failed:macos,review-verdict");
}

#[test]
fn a_hold_status_routes_to_the_repair_lane() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let mut pr = pr_with(
        12035,
        &[],
        "CLEAN",
        &json!([app_check(
            "macos",
            "COMPLETED",
            Some("SUCCESS"),
            "2026-11-11T11:20:00Z"
        )]),
    );
    pr.fact.checks.push(review_status(Some(
        "a HOLD naming head aaaaaaaaaaaa makes it un-approvable (held by pulp-review at comment \
         6788738268, posted 2026-11-11T06:40:00Z)",
    )));
    let decision = StewardDecision::RequiredFailed {
        contexts: vec!["review-verdict (app_id=15368)".to_owned()],
    };
    let (blocked, _) = classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation: None,
            error: None,
            approval: GateRead::Unread,
            enqueued_at: None,
            ejection: None,
        },
        &policy,
    );
    assert_eq!(blocked.blocked_on, "held:review-verdict");
    assert!(
        blocked
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("makes it un-approvable"))
    );
    let held_by = blocked
        .held_by
        .as_ref()
        .expect("holder parsed from the status");
    assert_eq!(held_by.holder, "pulp-review");
    assert_eq!(held_by.record, "comment");
    assert_eq!(held_by.id, Some(6_788_738_268));
    let next = crate::app::merge_steward_cmd::next_action::for_row(
        &crate::app::merge_steward_cmd::next_action::RowContext {
            repo: "owner/repo",
            base: "main",
            pr: &pr.fact,
            policy: &policy,
            error: None,
        },
        &blocked,
    );
    assert_eq!(
        next.actor,
        crate::app::merge_steward_cmd::next_action::Actor::Repair
    );
    assert!(next.actionable());
    assert!(next.action.contains("HOLD by pulp-review"), "{next:?}");
    assert!(next.action.contains("issuecomment-6788738268"), "{next:?}");
    assert!(next.action.contains("fix what it names"), "{next:?}");
    assert!(next.action.contains("push once"), "{next:?}");
}

#[test]
fn awaiting_verdict_statuses_route_to_the_reviewer() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    for description in [
        "awaiting reviewer verdict for head aaaaaaaaaaaa",
        "no reviewer verdict names head aaaaaaaaaaaa; it needs a verdict",
    ] {
        let mut pr = pr_with(
            12036,
            &[],
            "CLEAN",
            &json!([app_check(
                "macos",
                "COMPLETED",
                Some("SUCCESS"),
                "2026-11-11T11:20:00Z"
            )]),
        );
        pr.fact.checks.push(review_status(Some(description)));
        let decision = StewardDecision::RequiredFailed {
            contexts: vec!["review-verdict (app_id=15368)".to_owned()],
        };
        let (blocked, _) = classify(
            &RowFacts {
                pr: &pr.fact,
                decision: &decision,
                mutation: None,
                error: None,
                approval: GateRead::Unread,
                enqueued_at: None,
                ejection: None,
            },
            &policy,
        );
        assert_eq!(blocked.blocked_on, "no_verdict", "{description}");
        let next = crate::app::merge_steward_cmd::next_action::for_row(
            &crate::app::merge_steward_cmd::next_action::RowContext {
                repo: "owner/repo",
                base: "main",
                pr: &pr.fact,
                policy: &policy,
                error: None,
            },
            &blocked,
        );
        assert_eq!(
            next.actor,
            crate::app::merge_steward_cmd::next_action::Actor::Reviewer
        );
        assert!(!next.actionable());
    }
}

#[test]
fn an_unreadable_review_verdict_description_keeps_the_failed_fallback() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let mut pr = pr_with(
        12037,
        &[],
        "CLEAN",
        &json!([app_check(
            "macos",
            "COMPLETED",
            Some("SUCCESS"),
            "2026-11-11T11:20:00Z"
        )]),
    );
    let base_checks = pr.fact.checks.clone();
    for description in [
        None,
        Some("Needs agent: review-verdict (app_id=15368)"),
        // A HOLD that names another head says nothing about this one.
        Some(
            "a HOLD naming head bbbbbbbbbbbb makes it un-approvable (held by pulp-review at \
             comment 1, posted 2026-11-11T06:40:00Z)",
        ),
        Some("no reviewer verdict names head bbbbbbbbbbbb; it needs a verdict"),
    ] {
        pr.fact.checks.clone_from(&base_checks);
        pr.fact.checks.push(review_status(description));
        let decision = StewardDecision::RequiredFailed {
            contexts: vec!["review-verdict (app_id=15368)".to_owned()],
        };
        let (blocked, _) = classify(
            &RowFacts {
                pr: &pr.fact,
                decision: &decision,
                mutation: None,
                error: None,
                approval: GateRead::Unread,
                enqueued_at: None,
                ejection: None,
            },
            &policy,
        );
        assert_eq!(
            blocked.blocked_on, "required_failed:review-verdict",
            "{description:?}"
        );
        assert!(blocked.held_by.is_none(), "{description:?}");
    }
}

/// A gate read outranks the status description: the gate saw the record.
#[test]
fn a_gate_read_outranks_the_status_description() {
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let mut pr = pr_with(
        12038,
        &[],
        "CLEAN",
        &json!([app_check(
            "macos",
            "COMPLETED",
            Some("SUCCESS"),
            "2026-11-11T11:20:00Z"
        )]),
    );
    pr.fact.checks.push(review_status(Some(
        "a HOLD naming head aaaaaaaaaaaa makes it un-approvable (held by pulp-review at comment \
         1, posted 2026-11-11T06:40:00Z)",
    )));
    let decision = StewardDecision::RequiredFailed {
        contexts: vec!["review-verdict (app_id=15368)".to_owned()],
    };
    let stale = ApprovalBlocker::StaleHead {
        approved_head: "bbbbbbbbbbbb".to_owned(),
        at: None,
    };
    let (blocked, _) = classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation: None,
            error: None,
            approval: GateRead::Refused(&stale),
            enqueued_at: None,
            ejection: None,
        },
        &policy,
    );
    assert_eq!(blocked.blocked_on, "approval_excluded:stale_head");
}

/// The dry run reads the gate for a review-verdict-red head and names the
/// uncounted signer from a real APPROVE body.
#[cfg(unix)]
#[test]
fn a_dry_run_names_an_uncounted_signer_on_a_review_verdict_red_head() {
    let temp = tempfile::tempdir().expect("temp");
    let comment = r#"{"id":6550000000,"login":"shipyard-local[bot]","type":"Bot","association":"CONTRIBUTOR","at":"2026-10-31T18:30:00Z","body":"reviewer: pulp-review2\n<!-- session: Daniels-Mac-Studio-m3/08fb81d8 agent: pulp-review2 -->\nverdict: APPROVE head:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#;
    let actions = fake_gh(
        &temp,
        &format!(
            r#"
# CONFIG_ERROR: this fixture serves its own policy file
case "$*" in
  *"contents/.shipyard/config.toml"*) printf '%s\n' '[auto_merge]' 'reviewer_signers = ["pulp-review"]' ;;
  *"pulls/969/reviews"*) ;;
  *"issues/969/comments"*) printf '%s\n' '{comment}' ;;
  *"commits/{HEAD}"*"--jq"*) printf '%s\n' '2026-10-31T17:00:00Z' ;;
  *"isInMergeQueue"*) printf '%s' '{{"data":{{"repository":{{"pullRequest":{{"number":969,"state":"OPEN","headRefOid":"{HEAD}","isInMergeQueue":false,"mergeQueueEntry":null,"autoMergeRequest":null,"timelineItems":{{"pageInfo":{{"hasPreviousPage":false}},"nodes":[]}}}}}}}}}}' ;;
  *) echo "unexpected: $*" >&2; exit 2 ;;
esac
"#
        ),
    );
    let policy = app_bound_policy(&["macos", "review-verdict"]);
    let pr = pr_with(
        969,
        &[],
        "CLEAN",
        &json!([
            app_check(
                "macos",
                "COMPLETED",
                Some("SUCCESS"),
                "2026-10-31T18:00:00Z"
            ),
            app_check(
                "review-verdict",
                "COMPLETED",
                Some("FAILURE"),
                "2026-10-31T18:40:16Z"
            ),
        ]),
    );
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "Generous-Corp/pulp".to_owned();
    observation
        .required_checks
        .clone_from(&policy.required_checks);
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let mut reports = vec![PrReport {
        number: 969,
        head_sha: HEAD.to_owned(),
        decision,
        mutation: None,
        error: None,
        blocked: Stall::default(),
    }];
    crate::head_approval::with_host_review_default(true, || {
        blocked_on::annotate(
            &actions,
            &observation,
            &policy,
            &StewardLedger::default(),
            &BTreeMap::new(),
            true,
            &mut reports,
        );
    });
    assert_eq!(
        reports[0].blocked.blocked_on,
        "approval_excluded:uncounted_signer"
    );
    assert_eq!(
        reports[0].blocked.since.as_deref(),
        Some("2026-10-31T18:30:00Z")
    );
}

/// A label that outranks a held head keeps the HOLD visible.
#[test]
fn a_label_outranking_a_hold_keeps_held_by() {
    let pr = pr_with(15, &["owner-decision"], "CLEAN", &green_rollup());
    let hold = ApprovalBlocker::Held {
        holder: "infra-review2".to_owned(),
        record: "comment",
        id: Some(1),
        at: Some("2026-10-31T15:00:00Z".to_owned()),
    };
    let (stall, _) = row(&pr, Some("waiting_head_approval"), GateRead::Refused(&hold));
    assert_eq!(stall.blocked_on, "owner_decision");
    assert_eq!(
        stall.held_by.map(|held| held.holder),
        Some("infra-review2".to_owned())
    );
}

/// A report's gate read never writes the carry log: a head whose approval
/// carries across a rebase is judged carried, and `carries.jsonl` stays
/// byte-identical. Control: the admission evaluation of the same head does
/// log the carry, so the fixture reaches the carry path.
#[test]
fn a_reports_gate_read_writes_no_carry_log() {
    use crate::head_approval::tests::{History, by_default};
    let history = History::new(false);
    let head = history.rebase(|history| history.write("more.txt", b"more base\n"), |_| {});
    let guards = tempfile::tempdir().expect("guards");
    let log = guards.path().join("carries.jsonl");
    fs::write(&log, "{\"kind\":\"rebase\",\"earlier\":true}\n").expect("seed");
    let before = fs::read(&log).expect("log");
    let run_gh = |args: &[String]| history.gh(args);
    let mut pr = ready_pr();
    pr.fact.head_sha.clone_from(&head);
    pr.fact.number = 7;
    let mut observation = observation_for(pr.clone(), true);
    observation.repo = "danielraffel/tartci".to_owned();
    let mut policy = by_default("danielraffel/tartci");
    policy.rebase_carry = true;
    let queue = json!({"data": {"repository": {"pullRequest": {
        "number": 7, "state": "OPEN", "headRefOid": head, "isInMergeQueue": false,
        "mergeQueueEntry": null, "autoMergeRequest": null,
        "timelineItems": {"pageInfo": {"hasPreviousPage": false}, "nodes": []}}}}});
    let ledger = StewardLedger::default();
    let (gate, blocker) = crate::head_approval::with_guards_dir(guards.path(), || {
        crate::head_approval::with_host_review_default(true, || {
            crate::app::merge_steward_cmd::pr_mutations::report_gate_with(
                &run_gh,
                &observation,
                &pr,
                &policy,
                &queue,
                &ledger,
            )
        })
    })
    .expect("gate");
    assert!(gate.allows(), "{gate:?}");
    assert_eq!(blocker, None);
    assert_eq!(
        fs::read(&log).expect("log"),
        before,
        "a report wrote a carry"
    );
    crate::head_approval::with_guards_dir(guards.path(), || {
        crate::head_approval::with_host_review_default(true, || {
            crate::app::merge_steward_cmd::pr_mutations::evaluate_head_gate_with(
                &run_gh,
                &observation,
                &pr,
                &head,
                &policy,
                &queue,
                &ledger,
            )
        })
    })
    .expect("gate");
    assert_ne!(
        fs::read(&log).expect("log"),
        before,
        "control: admission logs the carry"
    );
}

/// An admitted PR reads `admitted` even when it carries a skip label.
#[test]
fn an_admission_outranks_every_label() {
    let pr = pr_with(16, &["owner-decision"], "CLEAN", &green_rollup());
    assert_eq!(token(&pr, Some("enqueued")), "admitted");
}

#[test]
fn a_head_carrying_held_commits_names_the_held_pull_request_and_the_commits() {
    let pr = pr_with(11369, &[], "CLEAN", &green_rollup());
    let policy = queue_policy();
    let decision = classify_pr(&pr.fact, &policy, &BTreeMap::new());
    let detail = "carries 2 commit(s) of #11361 not on main (c26a23be, a0148424)";
    let (stall, _) = classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation: Some("carries_unapproved_commits:#11361"),
            error: Some(detail),
            approval: GateRead::Admits,
            enqueued_at: None,
            ejection: None,
        },
        &policy,
    );
    assert_eq!(stall.blocked_on, "carries_unapproved_commits:#11361");
    assert_eq!(stall.blocked_detail.as_deref(), Some(detail));
}

#[test]
fn a_pr_refusal_with_no_repo_error_is_named_in_exit_reasons() {
    let pr = |number: u64, error: Option<&str>| PrReport {
        number,
        head_sha: HEAD.to_owned(),
        decision: StewardDecision::ArmMergeQueue,
        mutation: None,
        error: error.map(str::to_owned),
        blocked: Stall::default(),
    };
    let mut report =
        unreadable_repo_report("o/r", "main", &StewardLedger::default(), "boom".to_owned());
    report.errors.clear();
    report.prs = vec![
        pr(1, None),
        pr(7, Some("carries 1 commit(s) of #6 not on main")),
        pr(9, Some("dequeue deferred")),
    ];
    assert_eq!(exit_reasons(&report), vec!["pr_errors:#7,#9".to_owned()]);

    // Negative control: a repository with no failing field is unclassified,
    // never silently empty, and a healthy report serializes no field.
    report.prs = vec![pr(1, None)];
    assert_eq!(exit_reasons(&report), vec!["unclassified".to_owned()]);
    assert!(
        serde_json::to_value(&report)
            .expect("serializes")
            .get("exit_reasons")
            .is_none()
    );
}

/// #11698 at 03:24: `Enforce version & skill sync` red while `macos` still
/// ran. Under the guard the row reads failed, not pending; the decision still
/// waits (and never admits), so recovery waits for a settled head.
#[test]
fn a_red_required_check_outranks_a_pending_one_in_the_report() {
    let pr = pr_with(
        11698,
        &[],
        "CLEAN",
        &json!([
            {"__typename": "CheckRun", "name": "macos", "status": "IN_PROGRESS",
             "startedAt": "2026-11-07T03:20:00Z"},
            {"__typename": "CheckRun", "name": "Enforce version & skill sync",
             "status": "COMPLETED", "conclusion": "FAILURE",
             "completedAt": "2026-11-07T03:24:29Z"}
        ]),
    );
    let mut policy = queue_policy();
    policy.required_checks = ["macos", "Enforce version & skill sync"]
        .into_iter()
        .map(|context| RequiredCheck {
            context: context.to_owned(),
            app_id: None,
        })
        .collect();
    let read = |policy: &StewardPolicy| {
        let decision = classify_pr(&pr.fact, policy, &BTreeMap::new());
        assert!(matches!(decision, StewardDecision::WaitingRequired { .. }));
        classify(
            &RowFacts {
                pr: &pr.fact,
                decision: &decision,
                mutation: None,
                error: None,
                approval: GateRead::Unread,
                enqueued_at: None,
                ejection: None,
            },
            policy,
        )
        .0
    };
    policy.ejection_guards = crate::merge_steward::EjectionGuards::for_base(true);
    let guarded = read(&policy);
    assert_eq!(
        guarded.blocked_on,
        "required_failed:Enforce version & skill sync"
    );
    assert_eq!(guarded.since.as_deref(), Some("2026-11-07T03:24:29Z"));
    assert!(
        guarded
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("macos")),
        "{guarded:?}"
    );
    // Control: without the guard the same head reads pending.
    policy.ejection_guards = crate::merge_steward::EjectionGuards::default();
    assert_eq!(read(&policy).blocked_on, "required_pending");
}

#[test]
fn a_pr_error_is_serialized_with_its_kind() {
    let pr = |error: Option<&str>| PrReport {
        number: 1,
        head_sha: HEAD.to_owned(),
        decision: StewardDecision::ArmMergeQueue,
        mutation: None,
        error: error.map(str::to_owned),
        blocked: Stall::default(),
    };
    let kind = |error: Option<&str>| {
        let value = serde_json::to_value(pr(error)).expect("serialize");
        // The wire shape keeps every existing field.
        assert_eq!(value["number"], 1);
        assert!(value.get("blocked_on").is_some());
        value["error_kind"].clone()
    };
    for read in [
        "queue-arm-guard: cannot read live PR state: gh: HTTP 503 refusing to land it unreviewed",
        "pull-request handoff preflight failed: gh api repos/o/r/pulls/1 failed: gh: HTTP 503",
        "current-head check identities failed after one retry: gh api x timed out after 15000ms",
        "queue entry not judged, dequeue deferred: head approval unreadable: gh api y",
        "gh api failed: Remote end closed connection without response",
        "gh: HTTP 429 secondary rate limit",
    ] {
        assert_eq!(kind(Some(read)), "read", "{read}");
    }
    for policy in [
        "needs an exact-head approval",
        "carries 1 commit(s) of #6 not on main",
        "dequeue deferred",
    ] {
        assert_eq!(kind(Some(policy)), "policy", "{policy}");
    }
    // No error, no kind.
    assert!(kind(None).is_null());
    assert_eq!(classify_pr_error("HTTP 404 not found"), ErrorKind::Policy);
}

fn status(name: &str, description: &str) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        description: Some(description.to_owned()),
        ..review_status(None)
    }
}

fn classify_failed(pr: &ObservedPr, policy: &StewardPolicy, contexts: &[&str]) -> Stall {
    let decision = StewardDecision::RequiredFailed {
        contexts: contexts
            .iter()
            .map(|context| (*context).to_owned())
            .collect(),
    };
    classify(
        &RowFacts {
            pr: &pr.fact,
            decision: &decision,
            mutation: None,
            error: None,
            approval: GateRead::Unread,
            enqueued_at: None,
            ejection: None,
        },
        policy,
    )
    .0
}

/// Vellum #415 and #418: `release-path-review` fails only because counted
/// APPROVEs are missing. That is reviewer throughput: `awaiting_review`,
/// the reviewer's move, never `required_failed` (which a base-level episode
/// would reword as "wait for the base fix").
#[test]
fn approvals_missing_on_release_path_review_is_awaiting_review() {
    let policy = app_bound_policy(&["macos", "release-path-review", "review-verdict"]);
    let mut pr = pr_with(
        415,
        &[],
        "BLOCKED",
        &json!([app_check(
            "macos",
            "COMPLETED",
            Some("SUCCESS"),
            "2026-11-13T20:00:00Z"
        )]),
    );
    pr.fact.checks.push(status(
        "release-path-review",
        ".github/workflows/x.yml and 1 more: needs 2 counted APPROVEs of head aaaaaaaaaaaa from different signers and sessions; has 0",
    ));
    let stall = classify_failed(&pr, &policy, &["release-path-review (app_id=15368)"]);
    assert_eq!(stall.blocked_on, "awaiting_review:release-path-review");
    assert!(
        stall
            .blocked_detail
            .as_deref()
            .is_some_and(|detail| detail.contains("approvals_missing"))
    );
    let next = crate::app::merge_steward_cmd::next_action::for_row(
        &crate::app::merge_steward_cmd::next_action::RowContext {
            repo: "Generous-Corp/vellum",
            base: "main",
            pr: &pr.fact,
            policy: &policy,
            error: None,
        },
        &stall,
    );
    assert_eq!(
        next.actor,
        crate::app::merge_steward_cmd::next_action::Actor::Reviewer
    );
    assert!(next.action.contains("not a base fault"), "{}", next.action);
    // An unknown description stays red (fail closed), just not base-level.
    let mut unknown = pr.clone();
    unknown.fact.checks.pop();
    unknown.fact.checks.push(status(
        "release-path-review",
        "workflow file outside the allow-list",
    ));
    assert_eq!(
        classify_failed(&unknown, &policy, &["release-path-review (app_id=15368)"]).blocked_on,
        "required_failed:release-path-review"
    );
    // A real red beside it keeps the row red.
    assert_eq!(
        classify_failed(
            &pr,
            &policy,
            &["macos (app_id=15368)", "release-path-review (app_id=15368)"]
        )
        .blocked_on,
        "required_failed:macos,release-path-review"
    );
}

/// A HOLD on the head stays a HOLD, even beside approvals missing elsewhere.
#[test]
fn a_hold_stays_a_hold_beside_approvals_missing() {
    let policy = app_bound_policy(&["macos", "release-path-review", "review-verdict"]);
    let mut pr = pr_with(
        418,
        &[],
        "BLOCKED",
        &json!([app_check(
            "macos",
            "COMPLETED",
            Some("SUCCESS"),
            "2026-11-13T20:00:00Z"
        )]),
    );
    pr.fact.checks.push(review_status(Some(
        "a HOLD naming head aaaaaaaaaaaa makes it un-approvable (held by pulp-review at comment 1, posted 2026-11-13T20:00:00Z)",
    )));
    pr.fact.checks.push(status(
        "release-path-review",
        "x: needs 2 counted APPROVEs of head aaaaaaaaaaaa from different signers and sessions; has 1 (pulp-review)",
    ));
    let stall = classify_failed(
        &pr,
        &policy,
        &[
            "release-path-review (app_id=15368)",
            "review-verdict (app_id=15368)",
        ],
    );
    assert_eq!(stall.blocked_on, "held:review-verdict");
}
