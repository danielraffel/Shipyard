use super::*;

const HEAD: &str = "1111111111111111111111111111111111111111";
const OTHER: &str = "2222222222222222222222222222222222222222";

fn now() -> DateTime<Utc> {
    parse_time("2026-11-13T12:00:00Z").expect("time")
}

fn target() -> Target {
    Target::parse("acme/widgets#7").expect("target")
}

fn actor() -> Actor {
    let env = |name: &str| match name {
        "CLAUDE_CODE_SESSION_ID" => Some("caller-sid".to_owned()),
        "SHIPYARD_AGENT_ID" => Some("caller-agent".to_owned()),
        _ => None,
    };
    Actor::from_env("m3".to_owned(), &env)
}

fn pr() -> PrFacts {
    PrFacts {
        state: "open".to_owned(),
        merged: false,
        head: HEAD.to_owned(),
        base: "main".to_owned(),
        author: "shipyard-local[bot]".to_owned(),
        body: "Fix the thing.\n<!-- shipyard-owner: host=m5 client=claude-code sid=author-sid \
               agent=author-agent at=2026-11-13T10:00:00Z -->"
            .to_owned(),
        labels: vec![HOLD_LABEL.to_owned()],
    }
}

fn queue() -> QueueFacts {
    QueueFacts {
        head: HEAD.to_owned(),
        in_queue: false,
        auto_merge_armed: false,
    }
}

fn check(name: &str, status: &str, conclusion: Option<&str>) -> StewardCheck {
    StewardCheck {
        name: name.to_owned(),
        source: crate::merge_steward::StewardCheckSource::CheckRun,
        app_id: None,
        status: status.to_owned(),
        conclusion: conclusion.map(str::to_owned),
        run_id: None,
        observed_at: Some("2026-11-13T11:00:00Z".to_owned()),
        check_id: Some(1),
        description: None,
    }
}

fn rule(name: &str) -> RequiredCheck {
    RequiredCheck {
        context: name.to_owned(),
        app_id: None,
    }
}

fn green() -> CheckFacts {
    CheckFacts {
        required: vec![rule("build"), rule("test")],
        checks: vec![
            check("build", "COMPLETED", Some("SUCCESS")),
            check("test", "COMPLETED", Some("SUCCESS")),
        ],
        rulesets_only: None,
    }
}

fn comment(id: u64, login: &str, at: &str, body: &str) -> Value {
    json!({
        "id": id,
        "login": login,
        "type": if login.ends_with("[bot]") { "Bot" } else { "User" },
        "association": "NONE",
        "at": at,
        "body": body,
    })
}

fn stamped_approve(id: u64, head: &str, sid: &str, agent: &str) -> Value {
    comment(
        id,
        "shipyard-local[bot]",
        "2026-11-13T11:30:00Z",
        &format!(
            "Looks right.\n<!-- session: m1/{sid} agent: {agent} -->\nverdict: APPROVE head:{head}"
        ),
    )
}

fn retrigger_facts() -> RetriggerFacts {
    RetriggerFacts {
        pr: pr(),
        queue: queue(),
        checks: CheckFacts {
            required: vec![rule("build"), rule("test")],
            checks: vec![
                check("build", "COMPLETED", Some("SUCCESS")),
                check("test", "COMPLETED", Some("FAILURE")),
            ],
            rulesets_only: None,
        },
        comments: Vec::new(),
        owner_record: None,
    }
}

fn retrigger(facts: &RetriggerFacts) -> Result<Decision, Refusal> {
    decide_retrigger(
        &target(),
        facts,
        &actor(),
        "merge ref is stale after base moved",
        None,
        now(),
    )
}

fn approved_gate(id: u64) -> HeadGate {
    HeadGate::Approved {
        approval: Approval::Marker {
            login: "shipyard-local[bot]".to_owned(),
            comment_id: Some(id),
            performed_via_github_app_id: Some(99),
        },
    }
}

fn label_event(event: &str, at: &str) -> LabelEvent {
    LabelEvent {
        event: event.to_owned(),
        label: HOLD_LABEL.to_owned(),
        login: "shipyard-local[bot]".to_owned(),
        at: parse_time(at).expect("time"),
    }
}

fn hold_body(conditions: &str) -> String {
    format!("Holding with {HOLD_LABEL} until the base fix lands.\n{conditions}")
}

fn hold_facts(conditions: &str) -> ReleaseHoldFacts {
    ReleaseHoldFacts {
        pr: pr(),
        checks: green(),
        gate: approved_gate(50),
        comments: vec![
            comment(
                40,
                "shipyard-local[bot]",
                "2026-11-13T09:01:00Z",
                &hold_body(conditions),
            ),
            stamped_approve(50, HEAD, "reviewer-run-1", "reviewer"),
        ],
        events: vec![label_event("labeled", "2026-11-13T09:00:00Z")],
        merged: BTreeMap::from([("acme/base#3".to_owned(), true)]),
        decision: None,
        owner_record: None,
    }
}

fn release(facts: &ReleaseHoldFacts) -> Result<Decision, Refusal> {
    decide_release_hold(&target(), facts, &actor(), None, now())
}

fn refused(result: Result<Decision, Refusal>) -> &'static str {
    result.expect_err("refused").condition
}

#[test]
fn target_parses_exact_owner_repo_and_number() {
    assert_eq!(
        target(),
        Target {
            repo: "acme/widgets".to_owned(),
            pr: 7
        }
    );
    for bad in [
        "acme/widgets",
        "acme#7",
        "acme/widgets#",
        "acme/widgets#0",
        "acme/widgets#+7",
    ] {
        assert!(Target::parse(bad).is_err(), "{bad}");
    }
}

#[test]
fn retrigger_allows_a_clean_stale_head_and_names_its_evidence() {
    let decision = retrigger(&retrigger_facts()).expect("allowed");
    assert_eq!(decision.act, Act::Retrigger);
    assert_eq!(decision.head, HEAD);
    let evidence = decision.evidence.join("\n");
    assert!(evidence.contains("not in the merge queue"), "{evidence}");
    assert!(evidence.contains("no live repair claim"), "{evidence}");
    assert!(evidence.contains("2 of 2 required checks"), "{evidence}");
    assert!(
        evidence.contains("neither authored nor approved"),
        "{evidence}"
    );
}

#[test]
fn retrigger_refuses_when_the_head_moved() {
    let facts = retrigger_facts();
    let moved = decide_retrigger(
        &target(),
        &facts,
        &actor(),
        "stale merge ref here",
        Some(OTHER),
        now(),
    );
    assert_eq!(refused(moved), "head_moved");
    let mut facts = retrigger_facts();
    facts.queue.head = OTHER.to_owned();
    assert_eq!(refused(retrigger(&facts)), "head_moved");
}

#[test]
fn retrigger_refuses_a_queued_or_armed_pull_request() {
    let mut facts = retrigger_facts();
    facts.queue.in_queue = true;
    assert_eq!(refused(retrigger(&facts)), "queued");
    let mut facts = retrigger_facts();
    facts.queue.auto_merge_armed = true;
    assert_eq!(refused(retrigger(&facts)), "armed");
}

#[test]
fn retrigger_refuses_a_live_repair_claim_and_ignores_an_expired_or_untrusted_one() {
    let mut facts = retrigger_facts();
    facts.comments = vec![comment(
        9,
        "shipyard-local[bot]",
        "2026-11-13T11:00:00Z",
        "repair-claim: repair-lane until 2026-11-13T13:00:00Z",
    )];
    let refusal = retrigger(&facts).expect_err("live claim");
    assert_eq!(refusal.condition, "repair_claim_live");
    assert!(refusal.detail.contains("repair-lane"), "{}", refusal.detail);

    facts.comments = vec![comment(
        9,
        "shipyard-local[bot]",
        "2026-11-13T09:00:00Z",
        "repair-claim: repair-lane until 2026-11-13T11:00:00Z",
    )];
    assert!(retrigger(&facts).is_ok(), "an expired claim is not live");

    facts.comments = vec![comment(
        9,
        "drive-by",
        "2026-11-13T11:00:00Z",
        "repair-claim: someone until 2026-11-14T00:00:00Z",
    )];
    assert!(
        retrigger(&facts).is_ok(),
        "an outsider's claim is not trusted"
    );
}

#[test]
fn a_claim_whose_time_does_not_parse_is_live() {
    let comments = vec![comment(
        9,
        "shipyard-local[bot]",
        "2026-11-13T11:00:00Z",
        "repair-claim: repair-lane until soon",
    )];
    assert_eq!(live_repair_claims(&comments, now()).len(), 1);
}

#[test]
fn neither_act_runs_on_a_pull_request_the_caller_authored() {
    let mut facts = retrigger_facts();
    facts.pr.body = "x\n<!-- shipyard-owner: host=m3 client=claude-code sid=caller-sid at=2026-11-13T10:00:00Z -->".to_owned();
    assert_eq!(refused(retrigger(&facts)), "actor_authored");

    let mut facts = retrigger_facts();
    facts.pr.body = "x\n<!-- whence {\"prov\": {\"session\": \"caller-sid\"}} -->".to_owned();
    assert_eq!(refused(retrigger(&facts)), "actor_authored");

    let mut facts = retrigger_facts();
    facts.owner_record = crate::owner_registry::parse_marker(
        "acme/widgets",
        7,
        "<!-- shipyard-owner: host=m3 client=codex agent=caller-agent at=2026-11-13T10:00:00Z -->",
    );
    assert!(facts.owner_record.is_some());
    assert_eq!(refused(retrigger(&facts)), "actor_authored");

    let mut facts = hold_facts("hold-condition: green");
    facts.pr.body = "<!-- shipyard-owner: host=m3 client=claude-code sid=caller-sid at=2026-11-13T10:00:00Z -->".to_owned();
    assert_eq!(refused(release(&facts)), "actor_authored");
}

#[test]
fn neither_act_runs_on_a_pull_request_the_caller_approved() {
    let mut facts = retrigger_facts();
    facts.comments = vec![stamped_approve(12, OTHER, "other-sid", "caller-agent")];
    assert_eq!(refused(retrigger(&facts)), "actor_approved");

    let mut facts = hold_facts("hold-condition: green");
    facts.comments[1] = stamped_approve(50, HEAD, "caller-sid", "someone-else");
    assert_eq!(refused(release(&facts)), "actor_approved");
}

#[test]
fn an_unidentified_caller_is_refused() {
    let anonymous = Actor::from_env("m3".to_owned(), &|_| None);
    assert!(!anonymous.identified());
    let result = decide_retrigger(
        &target(),
        &retrigger_facts(),
        &anonymous,
        "stale merge ref here",
        None,
        now(),
    );
    assert_eq!(refused(result), "actor_unidentified");
    let unattributed = Actor::from_env("m3".to_owned(), &|name| {
        (name == "CLAUDE_CODE_SESSION_ID").then(|| "unattributed".to_owned())
    });
    assert!(!unattributed.identified());
}

#[test]
fn retrigger_refuses_a_closed_or_merged_pull_request_and_a_missing_reason() {
    let mut facts = retrigger_facts();
    facts.pr.state = "closed".to_owned();
    assert_eq!(refused(retrigger(&facts)), "not_open");
    let mut facts = retrigger_facts();
    facts.pr.merged = true;
    assert_eq!(refused(retrigger(&facts)), "not_open");
    let result = decide_retrigger(
        &target(),
        &retrigger_facts(),
        &actor(),
        " stale ",
        None,
        now(),
    );
    assert_eq!(refused(result), "reason_missing");
}

#[test]
fn retrigger_needs_a_finished_required_run_at_this_head() {
    let mut facts = retrigger_facts();
    facts.checks.checks.clear();
    assert_eq!(refused(retrigger(&facts)), "no_required_run_at_head");
    let mut facts = retrigger_facts();
    facts.checks.checks[1] = check("test", "IN_PROGRESS", None);
    assert_eq!(refused(retrigger(&facts)), "required_run_in_progress");
    let mut facts = retrigger_facts();
    facts.checks.required.clear();
    assert_eq!(refused(retrigger(&facts)), "required_checks_unknown");
}

#[test]
fn release_hold_allows_when_every_named_condition_is_met() {
    let facts = hold_facts(
        "hold-condition: merged acme/base#3\nhold-condition: after 2026-11-13T10:00:00Z\n\
         hold-condition: approved\nhold-condition: green",
    );
    let decision = release(&facts).expect("allowed");
    assert_eq!(decision.act, Act::ReleaseHold);
    let evidence = decision.evidence.join("\n");
    assert!(
        evidence.contains("all 2 required checks green"),
        "{evidence}"
    );
    assert!(
        evidence.contains("comment 50 (stamped reviewer-run-1)"),
        "{evidence}"
    );
    assert!(evidence.contains("`merged acme/base#3` met"), "{evidence}");
    assert!(
        evidence.contains("`after 2026-11-13T10:00:00Z` met"),
        "{evidence}"
    );
    assert!(
        decision.reason.contains("hold comment 40"),
        "{}",
        decision.reason
    );
}

#[test]
fn release_hold_refuses_a_red_or_missing_required_check() {
    let mut facts = hold_facts("hold-condition: green");
    facts.checks.checks[1] = check("test", "COMPLETED", Some("FAILURE"));
    let refusal = release(&facts).expect_err("red");
    assert_eq!(refusal.condition, "required_not_green");
    assert!(
        refusal.detail.contains("test (failure)"),
        "{}",
        refusal.detail
    );
    facts.checks.checks.pop();
    let refusal = release(&facts).expect_err("missing");
    assert!(
        refusal.detail.contains("test (missing)"),
        "{}",
        refusal.detail
    );
}

#[test]
fn release_hold_refuses_a_missing_or_uncounted_approval() {
    let mut facts = hold_facts("hold-condition: green");
    facts.gate = HeadGate::NotApproved {
        head: HEAD.to_owned(),
    };
    assert_eq!(refused(release(&facts)), "approval_missing");
    facts.gate = HeadGate::NotRequired;
    assert_eq!(refused(release(&facts)), "approval_missing");
    facts.gate = HeadGate::SelfApproved {
        head: HEAD.to_owned(),
        agent: "x".to_owned(),
    };
    assert_eq!(refused(release(&facts)), "approval_missing");
    facts.gate = HeadGate::Approved {
        approval: Approval::Carried {
            from_head: OTHER.to_owned(),
            approval: Box::new(Approval::Review {
                login: "rev".to_owned(),
                performed_via_github_app_id: None,
            }),
            rebase: false,
        },
    };
    assert_eq!(refused(release(&facts)), "approval_not_exact_head");
}

#[test]
fn release_hold_refuses_an_approval_without_a_ghapp_stamp() {
    let mut facts = hold_facts("hold-condition: green");
    facts.comments[1] = comment(
        50,
        "shipyard-local[bot]",
        "2026-11-13T11:30:00Z",
        &format!("verdict: APPROVE head:{HEAD}"),
    );
    assert_eq!(refused(release(&facts)), "approval_unstamped");
    facts.gate = HeadGate::Approved {
        approval: Approval::Marker {
            login: "shipyard-local[bot]".to_owned(),
            comment_id: None,
            performed_via_github_app_id: None,
        },
    };
    assert_eq!(refused(release(&facts)), "approval_unstamped");
}

#[test]
fn an_approving_review_on_the_exact_head_counts() {
    let mut facts = hold_facts("hold-condition: green");
    facts.gate = HeadGate::Approved {
        approval: Approval::Review {
            login: "maintainer".to_owned(),
            performed_via_github_app_id: None,
        },
    };
    assert!(release(&facts).is_ok());
}

#[test]
fn release_hold_refuses_an_unmet_unread_or_uncheckable_condition() {
    let facts = hold_facts("hold-condition: merged acme/base#4");
    assert_eq!(refused(release(&facts)), "hold_condition_unread");
    let mut facts = hold_facts("hold-condition: merged acme/base#3");
    facts.merged.insert("acme/base#3".to_owned(), false);
    assert_eq!(refused(release(&facts)), "hold_condition_unmet");
    let facts = hold_facts("hold-condition: after 2026-11-14T00:00:00Z");
    assert_eq!(refused(release(&facts)), "hold_condition_unmet");
    let facts = hold_facts("hold-condition: the owner says so");
    assert_eq!(refused(release(&facts)), "hold_condition_unchecked");
    let facts = hold_facts("until the base fix lands, then release");
    assert_eq!(refused(release(&facts)), "hold_conditions_unnamed");
}

#[test]
fn the_hold_comment_must_be_found_exactly_once() {
    let mut facts = hold_facts("hold-condition: green");
    facts.comments[0] = comment(
        40,
        "shipyard-local[bot]",
        "2026-11-13T10:00:00Z",
        &hold_body("hold-condition: green"),
    );
    assert_eq!(refused(release(&facts)), "hold_comment_missing");

    let mut facts = hold_facts("hold-condition: green");
    facts.comments.push(comment(
        41,
        "shipyard-local[bot]",
        "2026-11-13T09:05:00Z",
        &hold_body("hold-condition: after 2026-11-20T00:00:00Z"),
    ));
    assert_eq!(refused(release(&facts)), "hold_comment_ambiguous");

    let mut facts = hold_facts("hold-condition: green");
    facts
        .events
        .push(label_event("unlabeled", "2026-11-13T09:30:00Z"));
    assert_eq!(refused(release(&facts)), "hold_event_unread");
    facts.events.clear();
    assert_eq!(refused(release(&facts)), "hold_event_unread");
}

#[test]
fn release_hold_refuses_when_there_is_no_hold() {
    let mut facts = hold_facts("hold-condition: green");
    facts.pr.labels.clear();
    assert_eq!(refused(release(&facts)), "hold_label_absent");
}

#[test]
fn a_decision_reference_that_names_the_pull_request_replaces_the_conditions() {
    let mut facts = hold_facts("hold-condition: the owner says so");
    facts.events.clear();
    let reference = DecisionRef::parse("acme/planning:DECISIONS.md#widgets-7").expect("ref");
    facts.decision = Some((
        reference.clone(),
        "## 2026-11-13\nRelease acme/widgets#7 once green and approved.".to_owned(),
    ));
    let decision = release(&facts).expect("allowed by decision");
    assert_eq!(
        decision.reason,
        "decision acme/planning:DECISIONS.md#widgets-7"
    );

    facts.decision = Some((reference, "Release acme/widgets#70 instead.".to_owned()));
    assert_eq!(refused(release(&facts)), "decision_unrelated");
}

#[test]
fn a_decision_never_excuses_red_checks_or_a_missing_approval() {
    let mut facts = hold_facts("");
    facts.decision = Some((
        DecisionRef::parse("acme/planning:DECISIONS.md").expect("ref"),
        "acme/widgets#7".to_owned(),
    ));
    facts.gate = HeadGate::NotApproved {
        head: HEAD.to_owned(),
    };
    assert_eq!(refused(release(&facts)), "approval_missing");
    facts.gate = approved_gate(50);
    facts.checks.checks.clear();
    assert_eq!(refused(release(&facts)), "required_not_green");
}

#[test]
fn decision_references_name_a_decisions_file() {
    let url = DecisionRef::parse(
        "https://github.com/acme/planning/blob/main/architecture/DECISIONS-2026.md#x",
    )
    .expect("url");
    assert_eq!(url.repo, "acme/planning");
    assert_eq!(url.path, "architecture/DECISIONS-2026.md");
    assert_eq!(url.git_ref.as_deref(), Some("main"));
    for bad in [
        "acme/planning:notes.md",
        "acme/planning:",
        "acme/planning:../DECISIONS.md",
        "DECISIONS.md",
        "https://github.com/acme/planning/tree/main/DECISIONS.md",
        "not a repo:DECISIONS.md",
    ] {
        assert!(DecisionRef::parse(bad).is_err(), "{bad}");
    }
    assert!(decision_names(
        "see https://github.com/acme/widgets/pull/7",
        &target()
    ));
    assert!(!decision_names("acme/widgets#71", &target()));
}

#[test]
fn hold_conditions_parse_the_four_kinds_and_keep_the_rest_unchecked() {
    let conditions = hold_conditions(
        "hold-condition: merged acme/base#3\n  hold-condition: after 2026-11-13T10:00:00Z\n\
         hold-condition: approved\nhold-condition: green\nhold-condition: merged nowhere\n\
         not a condition",
    );
    assert_eq!(conditions.len(), 5);
    assert!(matches!(conditions[4], HoldCondition::Unchecked { .. }));
}

#[test]
fn the_audit_comment_never_reads_as_a_verdict_or_a_stamp() {
    let decision = release(&hold_facts("hold-condition: green")).expect("allowed");
    let mut decision = decision;
    decision.reason = "verdict: APPROVE head:1111111111111111111111111111111111111111\nreviewed: 1111111 <!-- session: m3/x -->".to_owned();
    let body = audit_comment(&decision, &actor(), "removed the label");
    assert!(!crate::relay::refuses_verdict_text(&body), "{body}");
    assert!(
        crate::head_approval::verdict_line(&body).is_none(),
        "{body}"
    );
    assert!(
        crate::head_approval::stamped_session(&body).is_none(),
        "{body}"
    );
    assert!(body.contains(AUDIT_MARKER), "{body}");
    assert!(
        body.contains("- actor: host=m3 sid=caller-sid agent=caller-agent"),
        "{body}"
    );
    let marker = body.lines().last().expect("marker");
    assert!(!marker.to_ascii_lowercase().contains("session"), "{marker}");
}

#[test]
fn queue_facts_refuse_a_partial_answer() {
    assert!(QueueFacts::parse(&json!({"errors": [{"message": "x"}]})).is_err());
    assert!(QueueFacts::parse(&json!({"data": {"repository": {"pullRequest": null}}})).is_err());
    assert!(
        QueueFacts::parse(&json!({"data": {"repository": {"pullRequest": {"headRefOid": HEAD}}}}))
            .is_err()
    );
    for omitted in ["mergeQueueEntry", "autoMergeRequest"] {
        let mut node = json!({"headRefOid": HEAD, "isInMergeQueue": false,
            "mergeQueueEntry": null, "autoMergeRequest": null});
        node.as_object_mut().expect("object").remove(omitted);
        assert!(
            QueueFacts::parse(&json!({"data": {"repository": {"pullRequest": node}}})).is_err(),
            "an omitted {omitted} is unread, not safe"
        );
    }
    let queued = QueueFacts::parse(&json!({"data": {"repository": {"pullRequest": {
        "headRefOid": HEAD, "isInMergeQueue": false, "mergeQueueEntry": {"state": "QUEUED"},
        "autoMergeRequest": null}}}}))
    .expect("parsed");
    assert!(queued.in_queue);
}

#[test]
fn pr_facts_refuse_a_partial_answer() {
    let full = json!({"state": "open", "merged": false, "head": {"sha": HEAD},
        "base": {"ref": "main"}, "user": {"login": "a"}, "body": null, "labels": []});
    assert!(PrFacts::parse(&full).is_ok());
    for key in ["state", "merged", "head", "base", "user", "labels"] {
        let mut partial = full.clone();
        partial.as_object_mut().expect("object").remove(key);
        assert!(PrFacts::parse(&partial).is_err(), "{key}");
    }
}

#[test]
fn evidence_says_when_the_required_set_came_from_rulesets_only() {
    let mut facts = hold_facts("hold-condition: green");
    facts.checks.rulesets_only = Some("HTTP 403".to_owned());
    let decision = release(&facts).expect("allowed");
    assert!(
        decision
            .evidence
            .iter()
            .any(|line| line.contains("required set from rulesets only")),
        "{:?}",
        decision.evidence
    );
}

#[test]
fn release_hold_refuses_an_empty_required_set_even_with_green_checks() {
    let mut facts = hold_facts("hold-condition: green");
    facts.checks.required.clear();
    assert_eq!(refused(release(&facts)), "required_checks_unknown");
}
