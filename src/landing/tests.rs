//! Every check here is paired: one fixture that must produce the finding and
//! one that must not. A detector exercised only against the case it is
//! supposed to catch has never been shown to discriminate, and the failure
//! this module exists to prevent is a *false negative* — reporting no merge
//! queue on a repository that has one — so the negative cell is the load
//! bearing half.

use serde_json::{Value, json};

use crate::fleet_service::Boundary;
use crate::landing::backlog;
use crate::landing::placement::{self, JobObservation, Placement};
use crate::landing::queue::{
    Payload, QueueInputs, determine_queue, determine_strict, enqueue_guidance,
};
use crate::landing::{SurfaceOutcome, SurfaceRead, Verdict};

/// A ruleset carrying an active merge queue on the default branch.
///
/// Values match what a real repository with a queue reports: the list call
/// that precedes this one carries no `rules` array at all, which is why the
/// detail call is not optional.
fn ruleset_with_queue() -> Value {
    json!({
        "id": 19_431_100,
        "name": "main-merge-queue",
        "target": "branch",
        "enforcement": "active",
        "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
        "rules": [{
            "type": "merge_queue",
            "parameters": {
                "merge_method": "MERGE",
                "max_entries_to_build": 3,
                "min_entries_to_merge": 1,
                "max_entries_to_merge": 5,
                "min_entries_to_merge_wait_minutes": 1,
                "grouping_strategy": "ALLGREEN",
                "check_response_timeout_minutes": 120
            }
        }]
    })
}

/// A ruleset on the same branch that carries no queue.
fn ruleset_without_queue() -> Value {
    json!({
        "id": 14_883_619,
        "name": "protect-main",
        "target": "branch",
        "enforcement": "active",
        "conditions": { "ref_name": { "include": ["~DEFAULT_BRANCH"], "exclude": [] } },
        "rules": [
            { "type": "deletion" },
            { "type": "non_fast_forward" },
            {
                "type": "required_status_checks",
                "parameters": { "required_status_checks": [{ "context": "macos" }] }
            }
        ]
    })
}

/// A real branch-protection payload. Note what is NOT in it: there is no key
/// of any kind describing a merge queue. This is the whole problem — the
/// endpoint answers `200 OK` and is simply silent.
fn protection_payload(strict: bool) -> Value {
    json!({
        "required_status_checks": {
            "strict": strict,
            "contexts": ["macos", "Enforce version & skill sync"],
            "checks": [
                { "context": "macos", "app_id": 15368 },
                { "context": "Enforce version & skill sync", "app_id": 15368 }
            ]
        },
        "required_pull_request_reviews": { "required_approving_review_count": 0 },
        "enforce_admins": { "enabled": false },
        "required_linear_history": { "enabled": false },
        "allow_force_pushes": { "enabled": false },
        "allow_deletions": { "enabled": false }
    })
}

/// A reference implementation of the tempting shortcut: decide queue presence
/// from branch protection alone.
///
/// It exists so the ruleset test can be shown to FAIL against it. A test that
/// has never been run against a wrong implementation has not been shown to be
/// capable of failing, and this specific wrong implementation is the one that
/// ships by default, because branch protection is the endpoint everybody
/// reaches for first.
fn protection_only_queue_verdict(protection: &Value) -> &'static str {
    let mentions_queue = protection
        .as_object()
        .is_some_and(|fields| fields.keys().any(|key| key.contains("queue")));
    if mentions_queue { "present" } else { "absent" }
}

fn verdict_name<T>(verdict: &Verdict<T>) -> &'static str {
    match verdict {
        Verdict::Present(_) => "present",
        Verdict::Absent => "absent",
        Verdict::Unknown { .. } => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Merge queue: present / absent
// ---------------------------------------------------------------------------

#[test]
fn ruleset_merge_queue_is_reported() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_with_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    let config = finding
        .verdict
        .present()
        .expect("an active merge_queue rule must be reported");
    assert_eq!(config.ruleset_name.as_deref(), Some("main-merge-queue"));
    assert_eq!(config.enforcement.as_deref(), Some("active"));
    assert_eq!(config.grouping_strategy.as_deref(), Some("ALLGREEN"));
    assert_eq!(config.merge_method.as_deref(), Some("MERGE"));
    assert_eq!(config.max_entries_to_merge, Some(5));
    assert_eq!(config.max_entries_to_build, Some(3));
    assert_eq!(config.check_response_timeout_minutes, Some(120));
}

#[test]
fn ruleset_without_a_queue_does_not_false_positive() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_without_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(
        verdict_name(&finding.verdict),
        "absent",
        "a ruleset with no merge_queue rule must not be read as a queue"
    );
}

#[test]
fn no_rulesets_at_all_is_absent_not_unknown() {
    let inputs = QueueInputs {
        rulesets: Some(Vec::new()),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(false)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "absent");
}

// ---------------------------------------------------------------------------
// The real-world case: protection is silent, the ruleset is live
// ---------------------------------------------------------------------------

#[test]
fn protection_silent_and_ruleset_active_reports_the_queue() {
    let protection = protection_payload(true);
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_with_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection.clone()),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);

    assert_eq!(
        verdict_name(&finding.verdict),
        "present",
        "branch protection cannot express a queue, so its silence must not override a ruleset \
         that reports one"
    );

    // The proof that this test can fail: the shortcut implementation, handed
    // the identical branch-protection payload, answers the opposite. If this
    // assertion ever stops holding, the two implementations agree and the
    // test above has stopped discriminating.
    assert_eq!(
        protection_only_queue_verdict(&protection),
        "absent",
        "the protection-only reference must disagree, or this fixture proves nothing"
    );
    assert_ne!(
        verdict_name(&finding.verdict),
        protection_only_queue_verdict(&protection),
        "reading rulesets must produce a different answer than reading protection alone"
    );

    // Branch protection must be recorded as structurally unable to answer,
    // not as a surface that looked and found nothing.
    let protection_surface = surfaces
        .iter()
        .find(|surface| surface.surface == "branch_protection")
        .expect("branch protection must appear in the provenance list");
    assert!(
        matches!(
            protection_surface.outcome,
            SurfaceOutcome::Inexpressible { .. }
        ),
        "protection must be recorded as inexpressible, got {:?}",
        protection_surface.outcome
    );
}

// ---------------------------------------------------------------------------
// Fail closed: unreadable is UNKNOWN, never absent
// ---------------------------------------------------------------------------

#[test]
fn unreadable_rulesets_report_unknown_not_absent() {
    let inputs = QueueInputs {
        rulesets: None,
        rulesets_error: Some((
            Boundary::Permission,
            "HTTP 403: Resource not accessible by integration".to_owned(),
        )),
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    match &finding.verdict {
        Verdict::Unknown { boundary, detail } => {
            assert_eq!(*boundary, Boundary::Permission);
            assert!(
                detail.contains("rulesets"),
                "the unknown must name the surface that could not be read: {detail}"
            );
        }
        other => panic!(
            "unreadable rulesets must be UNKNOWN, got {}",
            verdict_name(other)
        ),
    }
}

#[test]
fn readable_rulesets_with_a_failed_detail_call_report_unknown() {
    // Half a ruleset read is an unread one: the rule that matters may be in
    // the ruleset whose detail call failed.
    let inputs = QueueInputs {
        rulesets: None,
        rulesets_error: Some((Boundary::Transport, "ruleset 42: HTTP 502".to_owned())),
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "unknown");
}

#[test]
fn unreadable_graphql_alone_also_refuses_to_assert_absence() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_without_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::Unreadable(Boundary::Transport, "HTTP 502".to_owned()),
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(
        verdict_name(&finding.verdict),
        "unknown",
        "one readable surface finding nothing does not license absence while another is blind"
    );
}

#[test]
fn every_surface_read_and_none_found_is_absent() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_without_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::Json(Value::Null),
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "absent");
}

#[test]
fn graphql_queue_alone_is_enough_to_report_present() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_without_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::Json(json!({
            "configuration": {
                "mergeMethod": "SQUASH",
                "mergingStrategy": "HEADGREEN",
                "maximumEntriesToMerge": 4,
                "maximumEntriesToBuild": 2,
                "minimumEntriesToMerge": 1,
                "checkResponseTimeout": 7200
            }
        })),
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    let config = finding.verdict.present().expect("graphql queue must count");
    assert_eq!(config.merge_method.as_deref(), Some("SQUASH"));
    // GraphQL reports the timeout in seconds; the report normalizes to the
    // minutes every other surface uses.
    assert_eq!(config.check_response_timeout_minutes, Some(120));
    assert!(
        !finding.disagreements.is_empty(),
        "one surface finding a queue while another did not is a disagreement worth printing"
    );
}

// ---------------------------------------------------------------------------
// Ruleset targeting
// ---------------------------------------------------------------------------

#[test]
fn a_queue_on_another_branch_is_not_this_branch_s_queue() {
    let mut ruleset = ruleset_with_queue();
    ruleset["conditions"]["ref_name"]["include"] = json!(["refs/heads/release"]);
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "absent");
}

#[test]
fn default_branch_alias_resolves_against_the_repository_default() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_with_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    // `~DEFAULT_BRANCH` on a repo whose default is `main`, asked about
    // `develop`, is not a queue on `develop`.
    let finding = determine_queue(&inputs, "develop", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "absent");

    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "present");
}

#[test]
fn an_excluded_branch_is_not_covered() {
    let mut ruleset = ruleset_with_queue();
    ruleset["conditions"]["ref_name"]["include"] = json!(["~ALL"]);
    ruleset["conditions"]["ref_name"]["exclude"] = json!(["refs/heads/main"]);
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    assert_eq!(verdict_name(&finding.verdict), "absent");
}

// ---------------------------------------------------------------------------
// Strict protection and its consequence
// ---------------------------------------------------------------------------

fn queue_finding(present: bool) -> crate::landing::queue::QueueFinding {
    let inputs = QueueInputs {
        rulesets: Some(vec![if present {
            ruleset_with_queue()
        } else {
            ruleset_without_queue()
        }]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    determine_queue(&inputs, "main", Some("main"), &mut surfaces)
}

#[test]
fn strict_on_with_a_queue_names_the_treadmill() {
    let strict = determine_strict(
        &Payload::Json(protection_payload(true)),
        &queue_finding(true),
    );
    assert_eq!(strict.verdict, Verdict::Present(true));
    assert!(
        strict.implication.contains("treadmill"),
        "the consequence an agent needs is that one-at-a-time merging is a treadmill: {}",
        strict.implication
    );
    assert!(strict.implication.contains("Enqueue"));
}

#[test]
fn strict_off_does_not_name_the_treadmill() {
    let strict = determine_strict(
        &Payload::Json(protection_payload(false)),
        &queue_finding(true),
    );
    assert_eq!(strict.verdict, Verdict::Present(false));
    assert!(
        !strict.implication.contains("treadmill"),
        "with strict off, landing one pull request does not invalidate the others"
    );
}

#[test]
fn unreadable_protection_reports_unknown_strict() {
    let strict = determine_strict(
        &Payload::Unreadable(Boundary::Permission, "HTTP 403".to_owned()),
        &queue_finding(true),
    );
    assert!(strict.verdict.is_unknown());
}

// ---------------------------------------------------------------------------
// Enqueue guidance
// ---------------------------------------------------------------------------

#[test]
fn enqueue_uses_the_method_the_queue_declares() {
    let guidance = enqueue_guidance(&queue_finding(true));
    assert_eq!(guidance.action, "enqueue");
    assert_eq!(
        guidance.command.as_deref(),
        Some("gh pr merge <number> --auto --merge"),
        "a queue configured for MERGE must never be given --squash"
    );
    assert!(guidance.rationale.contains("MERGE"));
}

#[test]
fn a_squash_queue_gets_squash_not_merge() {
    let inputs = QueueInputs {
        rulesets: Some(vec![{
            let mut ruleset = ruleset_with_queue();
            ruleset["rules"][0]["parameters"]["merge_method"] = json!("SQUASH");
            ruleset
        }]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    let guidance = enqueue_guidance(&finding);
    assert_eq!(
        guidance.command.as_deref(),
        Some("gh pr merge <number> --auto --squash")
    );
}

#[test]
fn an_unknown_queue_refuses_to_name_a_command() {
    let inputs = QueueInputs {
        rulesets: None,
        rulesets_error: Some((Boundary::Permission, "HTTP 403".to_owned())),
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::NotConsulted,
    };
    let mut surfaces = Vec::new();
    let finding = determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    let guidance = enqueue_guidance(&finding);
    assert_eq!(guidance.action, "unknown");
    assert!(
        guidance.command.is_none(),
        "naming a command from an undetermined mechanism is the false confidence this avoids"
    );
}

#[test]
fn no_queue_says_merge_directly() {
    let guidance = enqueue_guidance(&queue_finding(false));
    assert_eq!(guidance.action, "merge");
    assert!(guidance.command.is_some());
}

// ---------------------------------------------------------------------------
// Placement: where a required check actually runs
// ---------------------------------------------------------------------------

/// A jobs payload mixing a self-hosted gate, a hosted gate, and a job that was
/// skipped and therefore carries no runner identity at all.
fn jobs_payload() -> String {
    json!({
        "jobs": [
            {
                "name": "macos",
                "runner_name": "studio-gate-01-22474-1",
                "runner_group_name": "Default",
                "labels": ["self-hosted", "macOS", "ARM64", "build-vm"],
                "conclusion": "success"
            },
            {
                "name": "Enforce version & skill sync",
                "runner_name": "GitHub Actions 1000112040",
                "runner_group_name": "GitHub Actions",
                "labels": ["ubuntu-latest"],
                "conclusion": "success"
            },
            {
                "name": "windows",
                "runner_name": null,
                "runner_group_name": null,
                "labels": ["windows-latest"],
                "conclusion": "skipped"
            }
        ]
    })
    .to_string()
}

#[test]
fn a_self_hosted_gate_is_reported_self_hosted() {
    let jobs = placement::parse_jobs(&jobs_payload(), 42);
    let checks = placement::classify_checks(&["macos".to_owned()], &jobs, 30, None);
    match &checks[0].placement {
        Placement::SelfHosted {
            runner_name,
            runner_group,
            run_id,
            ..
        } => {
            assert_eq!(runner_name, "studio-gate-01-22474-1");
            assert_eq!(runner_group, "Default");
            assert_eq!(*run_id, 42);
        }
        other => panic!("expected self-hosted, got {other:?}"),
    }
    assert!(
        checks[0].conflicts.is_empty(),
        "a self-hosted runner serving a job that asked for `self-hosted` is consistent"
    );
}

#[test]
fn a_hosted_gate_is_reported_hosted() {
    let jobs = placement::parse_jobs(&jobs_payload(), 42);
    let checks = placement::classify_checks(
        &["Enforce version & skill sync".to_owned()],
        &jobs,
        30,
        None,
    );
    assert!(
        matches!(checks[0].placement, Placement::GithubHosted { .. }),
        "got {:?}",
        checks[0].placement
    );
}

#[test]
fn a_skipped_job_is_no_evidence_not_a_placement() {
    let jobs = placement::parse_jobs(&jobs_payload(), 42);
    let checks = placement::classify_checks(&["windows".to_owned()], &jobs, 30, None);
    match &checks[0].placement {
        Placement::NoEvidence { detail } => assert!(
            detail.contains("skipped in every run read"),
            "a job that was found but never ran is a different fact from one that was never \
             found: {detail}"
        ),
        other => panic!("a job that never ran says nothing about where it would run: {other:?}"),
    }
}

#[test]
fn a_context_with_no_job_at_all_says_so() {
    let jobs = placement::parse_jobs(&jobs_payload(), 42);
    let checks = placement::classify_checks(&["nonexistent".to_owned()], &jobs, 30, None);
    match &checks[0].placement {
        Placement::NoEvidence { detail } => assert!(
            detail.contains("no job named"),
            "an absent job must not be described as a skipped one: {detail}"
        ),
        other => panic!("expected no-evidence, got {other:?}"),
    }
}

#[test]
fn unreadable_runs_report_unknown_placement_not_hosted() {
    let checks = placement::classify_checks(
        &["macos".to_owned()],
        &[],
        30,
        Some(&(Boundary::Permission, "HTTP 403".to_owned())),
    );
    assert!(
        matches!(checks[0].placement, Placement::Unknown { .. }),
        "got {:?}",
        checks[0].placement
    );
}

#[test]
fn a_hosted_runner_serving_a_self_hosted_request_is_flagged() {
    let jobs = vec![JobObservation {
        name: "macos".to_owned(),
        runner_name: Some("GitHub Actions 9".to_owned()),
        runner_group: Some("GitHub Actions".to_owned()),
        requested_labels: vec!["self-hosted".to_owned(), "macOS".to_owned()],
        run_id: 7,
    }];
    let checks = placement::classify_checks(&["macos".to_owned()], &jobs, 30, None);
    assert!(matches!(
        checks[0].placement,
        Placement::GithubHosted { .. }
    ));
    assert!(
        !checks[0].conflicts.is_empty(),
        "the runner identity is the fact and the label is the request; the mismatch is worth \
         printing rather than silently resolving"
    );
}

#[test]
fn required_contexts_come_from_both_protection_shapes() {
    let contexts = placement::required_contexts(&protection_payload(true));
    assert_eq!(
        contexts,
        vec![
            "Enforce version & skill sync".to_owned(),
            "macos".to_owned()
        ]
    );
}

#[test]
fn an_empty_protection_payload_yields_no_contexts() {
    let contexts = placement::required_contexts(&json!({}));
    assert!(contexts.is_empty());
}

// ---------------------------------------------------------------------------
// Backlog
// ---------------------------------------------------------------------------

fn backlog_payload(states: &[(&str, bool)]) -> Value {
    let nodes: Vec<Value> = states
        .iter()
        .enumerate()
        .map(|(index, (state, auto))| {
            json!({
                "number": index + 1,
                "isDraft": false,
                "mergeStateStatus": state,
                "autoMergeRequest": if *auto { json!({ "enabledAt": "2026-09-21T00:00:00Z" }) } else { Value::Null }
            })
        })
        .collect();
    json!({
        "totalCount": nodes.len(),
        "pageInfo": { "hasNextPage": false },
        "nodes": nodes
    })
}

#[test]
fn a_conflict_heavy_backlog_says_conflicts_not_capacity() {
    let finding = backlog::from_graphql(&backlog_payload(&[
        ("DIRTY", false),
        ("DIRTY", false),
        ("DIRTY", false),
    ]));
    assert_eq!(finding.by_state.get("dirty"), Some(&3));
    assert!(finding.interpretation.contains("merge conflicts"));
    assert!(
        !finding.interpretation.contains("treadmill"),
        "conflicts are not the treadmill; conflating them sends the reader to the wrong fix"
    );
}

#[test]
fn a_behind_heavy_backlog_names_the_treadmill() {
    let finding = backlog::from_graphql(&backlog_payload(&[
        ("BEHIND", true),
        ("BEHIND", true),
        ("BLOCKED", true),
    ]));
    assert_eq!(finding.by_state.get("behind"), Some(&2));
    assert_eq!(finding.auto_merge_enabled, Some(3));
    assert!(finding.interpretation.contains("treadmill"));
}

#[test]
fn an_unreadable_backlog_is_not_an_empty_one() {
    let finding = backlog::unreadable(Boundary::Transport, "HTTP 502".to_owned());
    assert!(finding.total.is_none());
    assert!(finding.by_state.is_empty());
    assert!(finding.unreadable.is_some());
    assert!(
        finding.interpretation.contains("could not be read"),
        "an empty count must not read as an empty backlog"
    );
}

#[test]
fn a_truncated_backlog_says_so() {
    let mut payload = backlog_payload(&[("CLEAN", true)]);
    payload["pageInfo"]["hasNextPage"] = json!(true);
    payload["totalCount"] = json!(250);
    let finding = backlog::from_graphql(&payload);
    assert!(finding.truncated);
    assert!(finding.interpretation.contains("lower bound"));
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

#[test]
fn every_consulted_surface_appears_in_the_report() {
    let inputs = QueueInputs {
        rulesets: Some(vec![ruleset_with_queue()]),
        rulesets_error: None,
        protection: Payload::Json(protection_payload(true)),
        graphql_queue: Payload::Json(Value::Null),
    };
    let mut surfaces: Vec<SurfaceRead> = Vec::new();
    determine_queue(&inputs, "main", Some("main"), &mut surfaces);
    let names: Vec<&str> = surfaces
        .iter()
        .map(|surface| surface.surface.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["rulesets", "branch_protection", "graphql_merge_queue"],
        "a verdict with no provenance is indistinguishable from a guess"
    );
}
