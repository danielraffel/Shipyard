"""Apply-plan + idempotency tests."""

from __future__ import annotations

from unittest.mock import patch

from shipyard.governance.apply import (
    ApplyAction,
    build_apply_plan,
    execute_apply_plan,
)
from shipyard.governance.compare import compute_drift
from shipyard.governance.github import RepoRef
from shipyard.governance.profiles import BranchProtectionRules, solo_profile


def _repo() -> RepoRef:
    return RepoRef(owner="me", name="r")


def _declared() -> BranchProtectionRules:
    return solo_profile(
        required_status_checks=("mac", "linux"),
    ).branch_protection


# ── build_apply_plan action selection ───────────────────────────────────


def test_plan_noop_when_fully_aligned() -> None:
    declared = _declared()
    report = compute_drift(
        branch="main",
        profile_rules=declared,
        declared_rules=declared,
        live_rules=declared,
    )
    plan = build_apply_plan(
        repo=_repo(), branch="main",
        declared_rules=declared, drift_report=report,
    )
    assert plan.action == ApplyAction.NOOP
    assert plan.is_noop is True


def test_plan_update_when_live_drifts() -> None:
    declared = _declared()
    live = declared.with_overrides(enforce_admins=True)
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=live,
    )
    plan = build_apply_plan(
        repo=_repo(), branch="main",
        declared_rules=declared, drift_report=report,
    )
    assert plan.action == ApplyAction.UPDATE
    assert plan.is_noop is False


def test_plan_create_when_live_unprotected() -> None:
    declared = _declared()
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=None,
    )
    plan = build_apply_plan(
        repo=_repo(), branch="main",
        declared_rules=declared, drift_report=report,
    )
    assert plan.action == ApplyAction.CREATE


def test_plan_includes_manual_followups_even_for_noop() -> None:
    """The followup checklist is always printed, including for noop plans."""
    declared = _declared()
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=declared,
    )
    plan = build_apply_plan(
        repo=_repo(), branch="main",
        declared_rules=declared, drift_report=report,
    )
    assert plan.manual_followups
    assert any("Immutable releases" in f for f in plan.manual_followups)


# ── execute_apply_plan ──────────────────────────────────────────────────


def _plan_for(report, declared):
    return build_apply_plan(
        repo=_repo(), branch="main",
        declared_rules=declared, drift_report=report,
    )


def test_execute_noop_issues_zero_api_calls() -> None:
    declared = _declared()
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=declared,
    )
    plan = _plan_for(report, declared)

    with patch("shipyard.governance.apply.put_branch_protection") as mock_put:
        result = execute_apply_plan(plan)
    assert result.executed is False
    assert result.error_message is None
    mock_put.assert_not_called()


def test_execute_update_issues_put() -> None:
    declared = _declared()
    live = declared.with_overrides(enforce_admins=True)  # drift
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=live,
    )
    plan = _plan_for(report, declared)

    with patch("shipyard.governance.apply.put_branch_protection") as mock_put:
        result = execute_apply_plan(plan)
    assert result.executed is True
    assert result.error_message is None
    mock_put.assert_called_once()


def test_execute_create_issues_put() -> None:
    declared = _declared()
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=None,
    )
    plan = _plan_for(report, declared)

    with patch("shipyard.governance.apply.put_branch_protection") as mock_put:
        result = execute_apply_plan(plan)
    assert result.executed is True
    mock_put.assert_called_once()


def test_dry_run_issues_zero_api_calls_even_when_drift_present() -> None:
    declared = _declared()
    live = declared.with_overrides(enforce_admins=True)
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=live,
    )
    plan = _plan_for(report, declared)

    with patch("shipyard.governance.apply.put_branch_protection") as mock_put:
        result = execute_apply_plan(plan, dry_run=True)
    assert result.executed is False
    mock_put.assert_not_called()


def test_execute_captures_api_error() -> None:
    from shipyard.governance.github import GovernanceApiError
    declared = _declared()
    live = declared.with_overrides(enforce_admins=True)
    report = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=live,
    )
    plan = _plan_for(report, declared)

    with patch(
        "shipyard.governance.apply.put_branch_protection",
        side_effect=GovernanceApiError("permission denied"),
    ):
        result = execute_apply_plan(plan)
    assert result.executed is False
    assert result.error_message is not None
    assert "permission denied" in result.error_message


def test_idempotency_second_execute_is_noop_when_state_aligned() -> None:
    """Re-running apply after a successful apply is a noop."""
    declared = _declared()

    # First execution: drift exists, apply is called
    initial_live = None  # unprotected
    report1 = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=initial_live,
    )
    plan1 = _plan_for(report1, declared)
    with patch("shipyard.governance.apply.put_branch_protection") as mock_put1:
        execute_apply_plan(plan1)
    assert mock_put1.call_count == 1

    # Second execution: live now matches declared, apply is a noop
    report2 = compute_drift(
        branch="main", profile_rules=declared,
        declared_rules=declared, live_rules=declared,
    )
    plan2 = _plan_for(report2, declared)
    assert plan2.is_noop
    with patch("shipyard.governance.apply.put_branch_protection") as mock_put2:
        result = execute_apply_plan(plan2)
    assert mock_put2.call_count == 0
    assert result.executed is False
