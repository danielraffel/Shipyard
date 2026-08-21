use std::process::ExitCode;

use serde_json::Value;
use toml::Table;

use super::post_validation::{
    ShipRenderState, green_not_merged, green_pending_merge_readiness,
    green_validation_state_missing, post_run_merge_state, render_green_pending_merge_readiness,
};
use super::render::{
    render_green_not_merged, render_green_not_merged_client_defect, render_green_not_merged_flaky,
    render_green_not_merged_head_superseded, render_json,
};
use super::{SHIP_EXIT_MERGE_CLIENT_DEFECT, SHIP_EXIT_VALIDATION_STATE_MISSING};
use crate::config::{LoadedConfig, LocalOverlaySource};
use crate::identity::RuntimeMode;
use crate::job::{Job, Priority, ValidationMode};
use crate::ship::ShipExecutionOutcome;
use crate::ship_state::{DispatchedRun, ShipState, ShipStateStore};

/// Issue #301 (2/3): the render must surface the underlying merge
/// error verbatim and point the user at the two unblocks
/// (re-ship after checks complete, OR `gh pr merge --auto`).
/// It must NOT claim "all green" — when this branch fires, Shipyard
/// only validated local lanes; GitHub branch protection rejected
/// the merge because GHA-hosted checks were still in flight.
#[test]
fn render_green_not_merged_surfaces_error_and_unblock_options() {
    let mut buf = Vec::<u8>::new();
    let err = "GraphQL: Pull request is not mergeable: Base branch was modified.";
    render_green_not_merged(&mut buf, 2020, err).expect("render");
    let out = String::from_utf8(buf).expect("utf8");
    assert!(
        out.contains("PR #2020"),
        "must name the PR number; got:\n{out}"
    );
    assert!(
        out.contains(err),
        "must surface the merge error verbatim; got:\n{out}"
    );
    assert!(
        !out.contains("All green"),
        "must NOT claim 'all green' when the merge attempt was rejected; got:\n{out}"
    );
    assert!(
        out.contains("shipyard ship --pr 2020"),
        "must hint at re-running shipyard ship; got:\n{out}"
    );
    assert!(
        out.contains("gh pr merge 2020 --squash --auto"),
        "must hint at native auto-merge as the second option; got:\n{out}"
    );
}

#[test]
fn ship_render_state_only_merged_returns_true_for_merged() {
    assert!(ShipRenderState::Merged.merged());
    assert!(!ShipRenderState::ValidationFailed.merged());
    assert!(!ShipRenderState::GreenNotMerged("err".to_owned()).merged());
    assert!(!ShipRenderState::GreenPendingMergeReadiness("pending".to_owned()).merged());
    assert!(!ShipRenderState::GreenNotMergedClientDefect("err".to_owned()).merged());
    assert!(
        !ShipRenderState::GreenNotMergedFlakyRequired {
            error: "err".to_owned(),
            red_contexts: vec!["macos".to_owned()],
        }
        .merged()
    );
}

/// The exact stderr from the `autoMergeRequest{id}` schema bug must land in
/// the client-defect state, not in the generic branch-protection hand-back.
#[test]
fn malformed_graphql_query_classifies_as_a_client_defect() {
    let err = "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'".to_owned();
    assert_eq!(
        green_not_merged(err.clone()),
        ShipRenderState::GreenNotMergedClientDefect(err)
    );
}

#[test]
fn genuine_merge_rejection_stays_a_generic_green_not_merged() {
    let err = "gh: Required status check \"macos\" is expected.".to_owned();
    assert_eq!(
        green_not_merged(err.clone()),
        ShipRenderState::GreenNotMerged(err)
    );
}

/// A green PR stalled by a Shipyard defect must be distinguishable from a red
/// one by exit code alone, while every pre-existing state keeps its code.
#[test]
fn client_defect_gets_a_distinct_nonzero_exit_code() {
    assert_eq!(
        format!("{:?}", ShipRenderState::Merged.exit_code()),
        format!("{:?}", ExitCode::SUCCESS)
    );
    assert_eq!(
        format!("{:?}", ShipRenderState::ValidationFailed.exit_code()),
        format!("{:?}", ExitCode::from(1))
    );
    assert_eq!(
        format!(
            "{:?}",
            ShipRenderState::GreenNotMerged("e".to_owned()).exit_code()
        ),
        format!("{:?}", ExitCode::SUCCESS)
    );
    assert_eq!(
        format!(
            "{:?}",
            ShipRenderState::GreenPendingMergeReadiness("pending".to_owned()).exit_code()
        ),
        format!("{:?}", ExitCode::SUCCESS)
    );
    assert_eq!(
        format!(
            "{:?}",
            ShipRenderState::GreenNotMergedClientDefect("e".to_owned()).exit_code()
        ),
        format!("{:?}", ExitCode::from(SHIP_EXIT_MERGE_CLIENT_DEFECT))
    );
    assert_eq!(
        format!(
            "{:?}",
            ShipRenderState::GreenValidationStateMissing("e".to_owned()).exit_code()
        ),
        format!("{:?}", ExitCode::from(SHIP_EXIT_VALIDATION_STATE_MISSING))
    );
    // Must not collide with validation-failed, or the distinction is lost.
    assert_ne!(SHIP_EXIT_MERGE_CLIENT_DEFECT, 0);
    assert_ne!(SHIP_EXIT_MERGE_CLIENT_DEFECT, 1);
    assert_ne!(SHIP_EXIT_VALIDATION_STATE_MISSING, 0);
    assert_ne!(SHIP_EXIT_VALIDATION_STATE_MISSING, 1);
    assert_ne!(
        SHIP_EXIT_VALIDATION_STATE_MISSING,
        SHIP_EXIT_MERGE_CLIENT_DEFECT
    );
}

#[test]
fn json_status_and_merge_error_separate_the_failure_modes() {
    assert_eq!(ShipRenderState::Merged.status(), "merged");
    assert_eq!(ShipRenderState::Merged.merge_error(), None);
    assert_eq!(
        ShipRenderState::ValidationFailed.status(),
        "validation_failed"
    );
    assert_eq!(ShipRenderState::ValidationFailed.merge_error(), None);

    let err = "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'";
    let defect = ShipRenderState::GreenNotMergedClientDefect(err.to_owned());
    assert_eq!(defect.status(), "green_not_merged_client_defect");
    assert_eq!(defect.merge_error().as_deref(), Some(err));

    let blocked = ShipRenderState::GreenNotMerged("blocked".to_owned());
    assert_eq!(blocked.status(), "green_not_merged");
    assert_eq!(blocked.merge_error().as_deref(), Some("blocked"));

    let pending = green_pending_merge_readiness(42, "required checks are in flight");
    assert_eq!(pending.status(), "green_pending_merge_readiness");
    assert!(
        pending
            .merge_error()
            .is_some_and(|detail| detail.contains("local validation passed"))
    );
    let missing = green_validation_state_missing(42);
    assert_eq!(missing.status(), "green_validation_state_missing");
    assert!(
        missing
            .merge_error()
            .is_some_and(|detail| detail.contains("local validation passed"))
    );

    // Shipyard refuses this one client-side, so there is no `gh` error to
    // quote — the envelope still has to carry a reason.
    let superseded = ShipRenderState::GreenNotMergedHeadSuperseded {
        validated: "aaaa".to_owned(),
        current: "bbbb".to_owned(),
    };
    assert_eq!(superseded.status(), "green_not_merged_head_superseded");
    let reason = superseded.merge_error().expect("reason");
    assert!(reason.contains("aaaa"), "must name the validated SHA");
    assert!(reason.contains("bbbb"), "must name the live SHA");

    // No two green-but-unmerged states may share a status tag.
    let tags = [
        blocked.status(),
        pending.status(),
        missing.status(),
        defect.status(),
        superseded.status(),
    ];
    assert_eq!(
        tags.len(),
        tags.iter().collect::<std::collections::BTreeSet<_>>().len(),
        "status tags must be distinct: {tags:?}"
    );
}

#[test]
fn foreground_json_uses_the_final_post_validation_state() {
    let state = ShipRenderState::GreenPendingMergeReadiness(
        "local validation passed; required checks are still in flight".to_owned(),
    );
    let outcome = ShipExecutionOutcome {
        job: Job::create(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "feature/readiness",
            Vec::new(),
            ValidationMode::Full,
            Priority::Normal,
        ),
        ship_state: ShipState::new(
            7751,
            "Generous-Corp/pulp",
            "feature/readiness",
            "main",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "policy-signature",
        ),
        resumed_existing_state: false,
        post_validation: None,
    };
    let mut output = Vec::new();
    render_json(&mut output, 7751, &outcome, &state, &[], None).expect("render JSON");
    let value: serde_json::Value = serde_json::from_slice(&output).expect("JSON envelope");
    assert_eq!(
        value
            .pointer("/post_validation/kind")
            .and_then(Value::as_str),
        Some("green_pending_merge_readiness")
    );
    assert_eq!(
        value
            .pointer("/post_validation/exit_code")
            .and_then(Value::as_u64),
        Some(0)
    );
    assert_eq!(
        value.get("status").and_then(Value::as_str),
        Some("green_pending_merge_readiness")
    );
}

#[test]
fn pending_merge_readiness_preserves_green_proof_and_forbids_a_rerun() {
    let mut buf = Vec::<u8>::new();
    let pending = green_pending_merge_readiness(7751, "required checks are still in flight");
    let ShipRenderState::GreenPendingMergeReadiness(detail) = pending else {
        panic!("pending readiness must be typed separately from validation failure");
    };
    render_green_pending_merge_readiness(&mut buf, 7751, &detail).expect("render");
    let output = String::from_utf8(buf).expect("utf8");
    assert!(output.contains("validation proof remains green"));
    assert!(output.contains("Do not rerun validation"));
    assert!(output.contains("shipyard wait pr 7751 --state green"));
    assert!(!output.contains("shipyard ship --pr"));
}

#[test]
fn passing_validation_survives_non_ready_post_run_ship_state() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = ShipStateStore::new(temp.path().join("ship")).expect("ship state");
    let mut state = ShipState::new(
        7751,
        "owner/repo",
        "feature/validation",
        "main",
        "a".repeat(40),
        "policy",
    );
    state.update_evidence("local-validation", "pending");
    state.dispatched_runs.push(DispatchedRun {
        target: "local-validation".to_owned(),
        provider: "github".to_owned(),
        run_id: "1".to_owned(),
        status: "in_progress".to_owned(),
        started_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        attempt: 1,
        last_heartbeat_at: None,
        phase: None,
        required: true,
    });
    store.save(&state).expect("pending state");
    let config = LoadedConfig {
        data: Table::new(),
        global_dir: temp.path().join("global"),
        project_dir: None,
        local_dir: None,
        local_overlay_source: LocalOverlaySource::None,
    };

    for readiness in ["pending", "fail"] {
        state.update_evidence("local-validation", readiness);
        store.save(&state).expect("readiness state");
        let outcome = post_run_merge_state(
            7751,
            temp.path(),
            &store,
            &config,
            RuntimeMode::Isolated,
            "owner/repo",
            true,
            &state,
            None,
            None,
            None,
        )
        .expect("merge readiness must not invalidate passing validation");
        assert!(matches!(
            outcome,
            ShipRenderState::GreenPendingMergeReadiness(_)
        ));
        assert_eq!(
            format!("{:?}", outcome.exit_code()),
            format!("{:?}", ExitCode::SUCCESS)
        );
    }
}

/// Shipyard refuses a superseded head itself — GitHub rejected nothing — so
/// the render must not send the reader to branch protection.
#[test]
fn head_superseded_render_does_not_blame_branch_protection() {
    let mut buf = Vec::<u8>::new();
    render_green_not_merged_head_superseded(&mut buf, 384, "aaaa111", "bbbb222").expect("render");
    let out = String::from_utf8(buf).expect("utf8");
    assert!(
        out.contains("aaaa111"),
        "must name validated SHA; got:\n{out}"
    );
    assert!(out.contains("bbbb222"), "must name live SHA; got:\n{out}");
    assert!(
        !out.contains("branch protection requires"),
        "must NOT blame branch protection; got:\n{out}"
    );
    assert!(
        out.contains("--adopt-head"),
        "must point at the re-ship that adopts the new head; got:\n{out}"
    );
}

/// The generic render blames branch protection, which is wrong for this
/// failure. The client-defect render must not repeat that misdirection.
#[test]
fn client_defect_render_blames_shipyard_not_branch_protection() {
    let mut buf = Vec::<u8>::new();
    let err = "gh: Field 'id' doesn't exist on type 'AutoMergeRequest'";
    render_green_not_merged_client_defect(&mut buf, 6682, err).expect("render");
    let out = String::from_utf8(buf).expect("utf8");
    assert!(out.contains("PR #6682"), "must name the PR; got:\n{out}");
    assert!(
        out.contains(err),
        "must surface the error verbatim; got:\n{out}"
    );
    assert!(
        out.contains("malformed GraphQL request"),
        "must name the actual cause; got:\n{out}"
    );
    assert!(
        out.contains("Shipyard defect"),
        "must attribute the fault to Shipyard; got:\n{out}"
    );
    assert!(
        !out.contains("branch protection requires"),
        "must NOT repeat the branch-protection misdirection; got:\n{out}"
    );
    // The queue owns the strategy on a queue-governed branch, so the
    // suggested unblock must not hardcode --squash.
    assert!(
        out.contains("gh pr merge 6682 --auto"),
        "must offer a strategy-free unblock; got:\n{out}"
    );
    assert!(
        !out.contains("--squash --auto"),
        "must not suggest a strategy the merge queue would refuse; got:\n{out}"
    );
}

#[test]
fn flaky_required_render_points_at_the_rescue_one_liner() {
    let mut out = Vec::new();
    render_green_not_merged_flaky(
        &mut out,
        2020,
        "base branch policy prohibits the merge",
        &["macos".to_owned()],
    )
    .expect("render");
    let text = String::from_utf8(out).expect("utf8");
    assert!(
        text.contains("shipyard rescue 2020 --rerun-failed"),
        "must hand the operator the one-liner rescue; got:\n{text}"
    );
    assert!(
        text.contains("macos"),
        "must name the flaky required check; got:\n{text}"
    );
    assert!(
        text.contains("flaky required leg"),
        "must explain the block is a flake, not a regression; got:\n{text}"
    );
    assert!(
        !text.contains("All green"),
        "must not claim all green; got:\n{text}"
    );
}
