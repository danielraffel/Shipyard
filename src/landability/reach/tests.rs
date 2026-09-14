//! Negative controls for the reachability classifier.
//!
//! Each detector below has a fixture that must produce the failing verdict and
//! a control on the **same** instrument and input that must not. A detector
//! never observed failing is not known to work — and the failure mode that
//! motivated all of this is a measurement aimed at the wrong target, which
//! does not error: it succeeds and returns empty.
//!
//! The pull-request captures are real. `spectr#120` was opened at
//! `00:58:04Z` against `fix/help-overlay-layout-and-scroll`, retargeted to
//! `main` at `01:00:53Z`, and got its first and only run at `03:46:32Z` —
//! 2 h 48 m later, and only because a human pushed an empty commit.

use chrono::{DateTime, TimeZone, Utc};

use super::*;
use crate::landability::trigger::parse_workflow_triggers;
use crate::landability::workflow::parse_workflow_jobs;

const SPECTR_ON: &str =
    include_str!("../../../tests/fixtures/triggers/spectr-m5-product-acceptance.yml");
const SPECTR_RUNS_ON_HEAD: &str =
    include_str!("../../../tests/fixtures/triggers/spectr-120-runs-on-head.json");

/// The real head SHA of the pull request these captures came from.
const HEAD_SHA: &str = "d8f07bcb30b12175b82b4c5b47b34354da0872bd";
const STACKED_BASE: &str = "fix/help-overlay-layout-and-scroll";

fn at(text: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(text)
        .expect("fixture timestamp")
        .with_timezone(&Utc)
}

/// spectr's gate, with a job whose rendered `name:` is the required context.
fn spectr_workflow() -> WorkflowUnderTest {
    let source = format!(
        "{SPECTR_ON}\n\njobs:\n  product-acceptance:\n    name: Spectr M5 product-acceptance gate\
         \n    runs-on: [self-hosted, macOS, ARM64]\n"
    );
    WorkflowUnderTest {
        path: ".github/workflows/m5-product-acceptance.yml".to_owned(),
        jobs: parse_workflow_jobs(&source),
        triggers: parse_workflow_triggers(&source),
        base_triggers: None,
    }
}

fn gate_context(protected: bool) -> RequiredContext {
    RequiredContext {
        name: "Spectr M5 product-acceptance gate".to_owned(),
        protected,
        shipyard_source: Some("[merge] require_platforms".to_owned()),
    }
}

struct Case {
    contexts: Vec<RequiredContext>,
    workflows: Vec<WorkflowUnderTest>,
    base: String,
    changed: Vec<String>,
    protection_readable: bool,
    evidence: Option<HeadEvidence>,
    allow: Vec<String>,
}

impl Case {
    fn spectr(base: &str) -> Self {
        Self {
            contexts: vec![gate_context(true)],
            workflows: vec![spectr_workflow()],
            base: base.to_owned(),
            changed: vec!["src/a.cpp".to_owned()],
            protection_readable: true,
            evidence: None,
            allow: Vec::new(),
        }
    }

    fn run(&self) -> Vec<ContextReachability> {
        assess_reachability(&ReachInput {
            contexts: &self.contexts,
            workflows: &self.workflows,
            base: &self.base,
            changed_paths: &self.changed,
            protection_readable: self.protection_readable,
            evidence: self.evidence.as_ref(),
            allow_unreachable: &self.allow,
        })
    }

    fn one(&self) -> ContextReachability {
        let mut all = self.run();
        assert_eq!(all.len(), 1, "expected exactly one context");
        all.remove(0)
    }
}

fn empty_evidence() -> HeadEvidence {
    HeadEvidence {
        number: 120,
        head_sha: HEAD_SHA.to_owned(),
        runs: Vec::new(),
        base_ref_changed_at: None,
        base_ref_changed_from: None,
        timeline_read: false,
    }
}

// ---------------------------------------------------------------------------
// T1 — R2, base exclusion. The verdict the 2026-09-14 incident needed.
// ---------------------------------------------------------------------------

#[test]
fn t1_a_stacked_base_is_base_excluded_and_names_the_clause() {
    let verdict = Case::spectr(STACKED_BASE).one();
    assert_eq!(verdict.verdict, Reachability::BaseExcluded);
    assert!(verdict.verdict.blocks(), "R2 must refuse the submission");
    assert!(!verdict.verdict.waiting_helps(), "waiting never fixes R2");
    let clause = verdict.clause.expect("a clause decided it");
    assert!(
        clause.contains("on.pull_request.branches: [main]"),
        "the clause must be quoted as written in the file: {clause}"
    );
    assert!(clause.contains(STACKED_BASE), "{clause}");
}

#[test]
fn t1_control_the_configured_base_is_not_excluded() {
    // Same instrument, same workflow, same everything but the base.
    let verdict = Case::spectr("main").one();
    assert!(
        !verdict.verdict.blocks(),
        "the control must NOT block, or T1 proves nothing: {:?}",
        verdict.verdict
    );
    assert_eq!(
        verdict.verdict,
        Reachability::Triggered {
            evidence: TriggerEvidence::StaticallyAdmitted
        }
    );
}

// ---------------------------------------------------------------------------
// T2 — R3, path exclusion, in both signs.
// ---------------------------------------------------------------------------

#[test]
fn t2_a_docs_only_diff_under_protection_blocks_forever() {
    let mut case = Case::spectr("main");
    case.changed = vec!["docs/x.md".to_owned(), "planning/y.md".to_owned()];
    let verdict = case.one();
    assert_eq!(
        verdict.verdict,
        Reachability::PathsExcluded { protected: true }
    );
    assert!(
        verdict.verdict.blocks(),
        "a path-filtered REQUIRED check stays Pending forever"
    );
    assert!(
        verdict
            .remedies
            .iter()
            .any(|remedy| remedy.contains("Pending forever")),
        "the remedy must say which sign this is: {:?}",
        verdict.remedies
    );
}

#[test]
fn t2_the_same_diff_unprotected_is_an_intended_skip() {
    let mut case = Case::spectr("main");
    case.contexts = vec![gate_context(false)];
    case.changed = vec!["docs/x.md".to_owned()];
    let verdict = case.one();
    assert_eq!(
        verdict.verdict,
        Reachability::PathsExcluded { protected: false }
    );
    assert!(
        !verdict.verdict.blocks(),
        "unprotected, the same clause is a designed skip and must not refuse"
    );
    // A specific clause outranks R0, but the unprotected fact is not lost.
    assert!(
        verdict
            .notes
            .iter()
            .any(|note| note.contains("merge with nothing run")),
        "{:?}",
        verdict.notes
    );
}

#[test]
fn t2_control_a_source_diff_is_admitted() {
    let mut case = Case::spectr("main");
    case.changed = vec!["src/a.cpp".to_owned()];
    assert!(!case.one().verdict.blocks());
}

// ---------------------------------------------------------------------------
// T3 — R5, the retarget hole, on the real capture.
// ---------------------------------------------------------------------------

#[test]
fn t3_retargeted_with_no_run_and_no_edited_is_r5() {
    let mut case = Case::spectr("main");
    case.evidence = Some(HeadEvidence {
        base_ref_changed_at: Some(at("2026-09-14T01:00:53Z")),
        base_ref_changed_from: Some(STACKED_BASE.to_owned()),
        timeline_read: true,
        ..empty_evidence()
    });
    let verdict = case.one();
    assert_eq!(
        verdict.verdict,
        Reachability::Retargeted {
            from: Some(STACKED_BASE.to_owned())
        }
    );
    assert!(verdict.verdict.blocks());
    assert!(
        verdict
            .remedies
            .iter()
            .any(|remedy| remedy.contains("git commit --allow-empty")),
        "R5 must PRINT the command, never run it: {:?}",
        verdict.remedies
    );
    assert!(
        !verdict
            .remedies
            .iter()
            .any(|remedy| remedy.contains("workflow run") || remedy.contains("dispatch --")),
        "R5 must never suggest a dispatch: {:?}",
        verdict.remedies
    );
}

#[test]
fn t3_control_the_post_push_capture_is_triggered() {
    // The real run that eventually appeared: `pull_request`, on this head.
    let mut case = Case::spectr("main");
    case.evidence = Some(HeadEvidence {
        runs: vec![HeadRun {
            workflow_path: ".github/workflows/m5-product-acceptance.yml".to_owned(),
            event: "pull_request".to_owned(),
            created_at: at("2026-09-14T03:46:32Z"),
            id: 34_803_741_815,
        }],
        base_ref_changed_at: Some(at("2026-09-14T01:00:53Z")),
        base_ref_changed_from: Some(STACKED_BASE.to_owned()),
        timeline_read: true,
        ..empty_evidence()
    });
    let verdict = case.one();
    assert_eq!(
        verdict.verdict,
        Reachability::Triggered {
            evidence: TriggerEvidence::Run {
                event: "pull_request".to_owned(),
                id: 34_803_741_815,
            }
        },
        "the control must flip to Triggered, or T3 is measuring nothing"
    );
    assert!(!verdict.verdict.blocks());
}

#[test]
fn t3_the_real_capture_has_an_empty_pull_requests_array() {
    // This is why nothing here may key on that association. Pulp's own
    // `vellum-freeze-recovery.yml` filters `.pull_requests[] | .number == $pr`
    // and would report NO RUN for this pull request.
    let value: serde_json::Value =
        serde_json::from_str(SPECTR_RUNS_ON_HEAD).expect("capture parses");
    let runs = value["workflow_runs"].as_array().expect("runs array");
    assert_eq!(runs.len(), 1, "the capture holds exactly one run");
    assert_eq!(runs[0]["event"], "pull_request");
    assert_eq!(runs[0]["head_sha"], HEAD_SHA);
    assert_eq!(
        runs[0]["pull_requests"].as_array().expect("array").len(),
        0,
        "a genuine pull_request run with an EMPTY pull_requests[]; key on head_sha only"
    );
    // And the classifier, fed exactly this, must see the run.
    let parsed: Vec<HeadRun> = runs
        .iter()
        .map(|run| HeadRun {
            workflow_path: run["path"].as_str().expect("path").to_owned(),
            event: run["event"].as_str().expect("event").to_owned(),
            created_at: at(run["created_at"].as_str().expect("created_at")),
            id: run["id"].as_u64().expect("id"),
        })
        .collect();
    let mut case = Case::spectr("main");
    case.evidence = Some(HeadEvidence {
        runs: parsed,
        ..empty_evidence()
    });
    assert!(matches!(
        case.one().verdict,
        Reachability::Triggered {
            evidence: TriggerEvidence::Run { .. }
        }
    ));
}

#[test]
fn a_run_on_the_head_outranks_an_undecidable_static_clause() {
    // A run that exists is a fact; every static clause is a prediction that
    // one would be created. Classifying `--pr N` from a checkout sitting on
    // another branch gives an empty diff, which makes the path filter
    // undecidable — and reporting Unknown about a pull request whose run is
    // sitting right there is exactly the half-answer this module exists to
    // end.
    let mut case = Case::spectr("main");
    case.changed = Vec::new();
    case.evidence = Some(HeadEvidence {
        runs: vec![HeadRun {
            workflow_path: ".github/workflows/m5-product-acceptance.yml".to_owned(),
            event: "pull_request".to_owned(),
            created_at: at("2026-09-14T03:46:32Z"),
            id: 34_803_741_815,
        }],
        ..empty_evidence()
    });
    assert!(matches!(
        case.one().verdict,
        Reachability::Triggered {
            evidence: TriggerEvidence::Run { .. }
        }
    ));

    // Control: the same undecidable diff with NO run must stay Unknown, or
    // the test above proves only that the classifier always says Triggered.
    let mut control = Case::spectr("main");
    control.changed = Vec::new();
    assert!(matches!(
        control.one().verdict,
        Reachability::Unknown { .. }
    ));
}

// ---------------------------------------------------------------------------
// T4 — R6, the workflow_dispatch trap.
// ---------------------------------------------------------------------------

#[test]
fn t4_a_dispatch_is_wrong_evidence_and_is_never_counted() {
    let mut case = Case::spectr("main");
    case.evidence = Some(HeadEvidence {
        runs: vec![HeadRun {
            workflow_path: ".github/workflows/m5-product-acceptance.yml".to_owned(),
            event: "workflow_dispatch".to_owned(),
            created_at: at("2026-09-14T02:00:00Z"),
            id: 1,
        }],
        ..empty_evidence()
    });
    let verdict = case.one();
    assert_eq!(
        verdict.verdict,
        Reachability::WrongEvidence {
            events: vec!["workflow_dispatch".to_owned()]
        }
    );
    assert!(
        verdict.verdict.blocks(),
        "a dispatch must never satisfy a pull-request requirement"
    );
    assert!(
        verdict
            .remedies
            .iter()
            .any(|remedy| remedy.contains("push a commit")),
        "the remedy for absence is a push, never another dispatch: {:?}",
        verdict.remedies
    );
}

#[test]
fn t4_control_the_same_run_under_a_pull_request_event_is_accepted() {
    let mut case = Case::spectr("main");
    case.evidence = Some(HeadEvidence {
        runs: vec![HeadRun {
            workflow_path: ".github/workflows/m5-product-acceptance.yml".to_owned(),
            event: "pull_request".to_owned(),
            created_at: at("2026-09-14T02:00:00Z"),
            id: 1,
        }],
        ..empty_evidence()
    });
    assert!(!case.one().verdict.blocks());
}

// ---------------------------------------------------------------------------
// T5 — R0, required by Shipyard and by nothing else.
// ---------------------------------------------------------------------------

#[test]
fn t5_a_shipyard_only_requirement_on_an_unprotected_branch_is_not_required() {
    let mut case = Case::spectr("main");
    case.contexts = vec![gate_context(false)];
    let verdict = case.one();
    assert_eq!(verdict.verdict, Reachability::NotRequired);
    assert!(
        !verdict.verdict.blocks(),
        "nothing is broken on the pull request; the finding is about the repository"
    );
    let warnings = warnings(&[verdict]);
    assert!(
        warnings
            .iter()
            .any(|warning| warning.contains("merge with nothing run")),
        "the half that matters must be stated: {warnings:?}"
    );
}

#[test]
fn t5_control_the_same_context_under_protection_is_not_r0() {
    let verdict = Case::spectr("main").one();
    assert_ne!(verdict.verdict, Reachability::NotRequired);
}

#[test]
fn t5_an_unreadable_protection_is_not_an_unprotected_branch() {
    // The distinction the gather layer keeps: "GitHub says nothing is
    // required" is a finding; "the call failed" is a statement about the
    // instrument. Folding them together turns every transport blip into a
    // confident claim that the branch is unprotected.
    let mut case = Case::spectr("main");
    case.contexts = vec![gate_context(false)];
    case.protection_readable = false;
    assert_ne!(case.one().verdict, Reachability::NotRequired);
}

// ---------------------------------------------------------------------------
// R1 / R4 — no producer, and a workflow that cannot report on a pull request.
// ---------------------------------------------------------------------------

#[test]
fn a_context_no_workflow_produces_is_r1_and_says_it_was_not_checked() {
    let mut case = Case::spectr("main");
    case.contexts = vec![RequiredContext {
        name: "Some External App".to_owned(),
        protected: true,
        shipyard_source: None,
    }];
    let verdict = case.one();
    assert_eq!(verdict.verdict, Reachability::NoProducer);
    assert!(
        !verdict.verdict.blocks(),
        "an external producer is not a trigger fault"
    );
    assert!(
        warnings(&[verdict])
            .iter()
            .any(|warning| warning.contains("NOT checked")),
        "silence about an unchecked context is the failure mode"
    );
}

#[test]
fn a_dispatch_only_workflow_is_event_excluded() {
    let source = "on:\n  workflow_dispatch:\n\njobs:\n  gate:\n    name: The Gate\n    runs-on: \
                  ubuntu-latest\n";
    let mut case = Case::spectr("main");
    case.contexts = vec![RequiredContext {
        name: "The Gate".to_owned(),
        protected: true,
        shipyard_source: None,
    }];
    case.workflows = vec![WorkflowUnderTest {
        path: ".github/workflows/dispatch-only.yml".to_owned(),
        jobs: parse_workflow_jobs(source),
        triggers: parse_workflow_triggers(source),
        base_triggers: None,
    }];
    let verdict = case.one();
    assert_eq!(verdict.verdict, Reachability::EventExcluded);
    assert!(verdict.verdict.blocks());
}

#[test]
fn a_merge_group_only_workflow_never_reports_on_the_pull_request() {
    let source = "on:\n  merge_group:\n\njobs:\n  gate:\n    name: The Gate\n    runs-on: \
                  ubuntu-latest\n";
    let mut case = Case::spectr("main");
    case.contexts = vec![RequiredContext {
        name: "The Gate".to_owned(),
        protected: true,
        shipyard_source: None,
    }];
    case.workflows = vec![WorkflowUnderTest {
        path: ".github/workflows/mg.yml".to_owned(),
        jobs: parse_workflow_jobs(source),
        triggers: parse_workflow_triggers(source),
        base_triggers: None,
    }];
    assert_eq!(case.one().verdict, Reachability::EventExcluded);
}

// ---------------------------------------------------------------------------
// Unknown, waivers, ordering, and the refusal text itself.
// ---------------------------------------------------------------------------

#[test]
fn a_refused_on_block_is_unknown_and_never_a_pass_or_a_refusal() {
    let source = "on:\n  pull_request:\n    branches: [\"${{ vars.B }}\"]\n\njobs:\n  gate:\n    \
                  name: The Gate\n    runs-on: ubuntu-latest\n";
    let mut case = Case::spectr("main");
    case.contexts = vec![RequiredContext {
        name: "The Gate".to_owned(),
        protected: true,
        shipyard_source: None,
    }];
    case.workflows = vec![WorkflowUnderTest {
        path: ".github/workflows/x.yml".to_owned(),
        jobs: parse_workflow_jobs(source),
        triggers: parse_workflow_triggers(source),
        base_triggers: None,
    }];
    let verdict = case.one();
    assert!(matches!(verdict.verdict, Reachability::Unknown { .. }));
    assert!(
        !verdict.verdict.blocks(),
        "a blind instrument must not refuse"
    );
    assert!(
        warnings(&[verdict])
            .iter()
            .any(|warning| warning.contains("UNKNOWN")),
        "and must not be silent either"
    );
}

#[test]
fn the_waiver_covers_only_the_trigger_and_only_the_named_workflow() {
    let mut case = Case::spectr(STACKED_BASE);
    case.allow = vec!["m5-product-acceptance.yml".to_owned()];
    let verdict = case.one();
    assert!(verdict.waived);
    assert!(
        render_refusal(std::slice::from_ref(&verdict)).is_empty(),
        "a waived context must not produce a refusal"
    );
    assert!(
        warnings(&[verdict])
            .iter()
            .any(|warning| warning.contains("WAIVED")),
        "but it must still be printed"
    );

    let mut other = Case::spectr(STACKED_BASE);
    other.allow = vec!["some-other-workflow.yml".to_owned()];
    let verdict = other.one();
    assert!(
        !verdict.waived,
        "a waiver must not spill onto other workflows"
    );
    assert!(!render_refusal(&[verdict]).is_empty());
}

#[test]
fn the_refusal_names_the_clause_the_remedies_and_the_contract() {
    let text = render_refusal(&Case::spectr(STACKED_BASE).run());
    for expected in [
        "will not be requested",
        "base_excluded",
        ".github/workflows/m5-product-acceptance.yml",
        "on.pull_request.branches: [main]",
        "this tool performs none",
        "contract [default] #4: nothing re-dispatched.",
        "--allow-unserved-lane does NOT cover this",
    ] {
        assert!(text.contains(expected), "missing `{expected}` in:\n{text}");
    }
}

#[test]
fn severity_ranks_the_earliest_broken_link_worst() {
    // Fixing a later link changes nothing while an earlier one is broken, so
    // the summary must lead with the earliest.
    let order = [
        Reachability::Triggered {
            evidence: TriggerEvidence::StaticallyAdmitted,
        },
        Reachability::NotRequired,
        Reachability::NoProducer,
        Reachability::Unknown {
            boundary: String::new(),
            detail: String::new(),
        },
        Reachability::PathsExcluded { protected: false },
        Reachability::WrongEvidence { events: Vec::new() },
        Reachability::Retargeted { from: None },
        Reachability::PathsExcluded { protected: true },
        Reachability::BaseExcluded,
        Reachability::EventExcluded,
    ];
    for pair in order.windows(2) {
        assert!(
            pair[0].severity() < pair[1].severity(),
            "{} must rank below {}",
            pair[0].as_str(),
            pair[1].as_str()
        );
    }
    assert!(order.iter().filter(|verdict| verdict.blocks()).count() == 5);
}

#[test]
fn only_one_verdict_is_ever_resolved_by_waiting() {
    let mut case = Case::spectr("main");
    case.evidence = Some(HeadEvidence {
        timeline_read: true,
        ..empty_evidence()
    });
    let verdict = case.one();
    assert!(
        verdict.verdict.waiting_helps(),
        "admitted with no run yet is the ONE state waiting resolves: {:?}",
        verdict.verdict
    );
    for other in [
        Reachability::BaseExcluded,
        Reachability::EventExcluded,
        Reachability::Retargeted { from: None },
        Reachability::WrongEvidence { events: Vec::new() },
        Reachability::PathsExcluded { protected: true },
        Reachability::NotRequired,
        Reachability::NoProducer,
    ] {
        assert!(
            !other.waiting_helps(),
            "{} must not promise that waiting helps",
            other.as_str()
        );
    }
}

#[test]
fn an_unread_timeline_is_not_evidence_that_no_retarget_happened() {
    let mut case = Case::spectr("main");
    case.evidence = Some(empty_evidence());
    // timeline_read: false — so R5 must not be claimed, and must not be
    // silently ruled out either: the verdict falls back to "awaiting run".
    assert!(!matches!(
        case.one().verdict,
        Reachability::Retargeted { .. }
    ));
}

#[test]
fn a_head_base_trigger_divergence_is_reported() {
    let head = "on:\n  pull_request:\n    branches: [main]\n\njobs:\n  gate:\n    name: The Gate\n \
                   runs-on: ubuntu-latest\n";
    let base = "on:\n  pull_request:\n    branches: [dev]\n";
    let mut case = Case::spectr("main");
    case.contexts = vec![RequiredContext {
        name: "The Gate".to_owned(),
        protected: true,
        shipyard_source: None,
    }];
    case.workflows = vec![WorkflowUnderTest {
        path: ".github/workflows/x.yml".to_owned(),
        jobs: parse_workflow_jobs(head),
        triggers: parse_workflow_triggers(head),
        base_triggers: Some(parse_workflow_triggers(base)),
    }];
    let verdict = case.one();
    assert!(
        verdict
            .notes
            .iter()
            .any(|note| note.contains("different triggers")),
        "a pull request that edits its own trigger is exactly the doubtful case: {:?}",
        verdict.notes
    );
}

#[test]
fn t7_a_producer_without_edited_warns_that_a_retarget_will_not_refire_it() {
    let verdict = Case::spectr("main").one();
    assert!(
        verdict
            .notes
            .iter()
            .any(|note| note.contains("does not declare `edited`")),
        "{:?}",
        verdict.notes
    );

    // Control: the same classifier on a workflow that DOES declare it.
    let source = "on:\n  pull_request:\n    branches: [main]\n    types: [opened, synchronize, \
                  reopened, edited]\n\njobs:\n  gate:\n    name: The Gate\n    runs-on: \
                  ubuntu-latest\n";
    let mut case = Case::spectr("main");
    case.contexts = vec![RequiredContext {
        name: "The Gate".to_owned(),
        protected: true,
        shipyard_source: None,
    }];
    case.workflows = vec![WorkflowUnderTest {
        path: ".github/workflows/x.yml".to_owned(),
        jobs: parse_workflow_jobs(source),
        triggers: parse_workflow_triggers(source),
        base_triggers: None,
    }];
    assert!(
        !case
            .one()
            .notes
            .iter()
            .any(|note| note.contains("does not declare `edited`")),
        "the control must be silent, or the warning proves nothing"
    );
}

#[test]
fn worst_picks_the_earliest_broken_link_across_contexts() {
    let mut case = Case::spectr(STACKED_BASE);
    case.contexts = vec![
        gate_context(true),
        RequiredContext {
            name: "Some External App".to_owned(),
            protected: true,
            shipyard_source: None,
        },
    ];
    let all = case.run();
    assert_eq!(all.len(), 2);
    assert_eq!(
        worst(&all).expect("a worst").verdict,
        Reachability::BaseExcluded
    );
}

#[test]
fn the_epoch_helper_is_not_used_for_anything_load_bearing() {
    // Guards the fixture helper itself: a timestamp parser that silently
    // returned the epoch would make every retarget comparison meaningless.
    assert_eq!(
        at("2026-09-14T01:00:53Z"),
        Utc.with_ymd_and_hms(2026, 9, 14, 1, 0, 53).unwrap()
    );
}
