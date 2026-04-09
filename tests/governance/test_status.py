"""Tests for the high-level `build_status` and `format_status_text` rollup."""

from __future__ import annotations

from unittest.mock import patch

from shipyard.core.config import Config
from shipyard.governance.config import load_governance_config
from shipyard.governance.github import RepoRef
from shipyard.governance.status import build_status, format_status_text


def _solo_config() -> Config:
    return Config(data={
        "project": {"profile": "solo"},
        "governance": {"required_status_checks": ["mac", "linux", "windows"]},
    })


def _rules_solo():
    """Build the rules that the solo-with-checks config resolves to."""
    from shipyard.governance.config import resolve_branch_rules
    return resolve_branch_rules(load_governance_config(_solo_config()), "main")


# ── build_status happy path: aligned ────────────────────────────────────


def test_build_status_aligned_has_no_drift() -> None:
    gov = load_governance_config(_solo_config())
    declared = _rules_solo()
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=declared,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    assert status.has_drift is False
    assert status.has_errors is False
    assert len(status.reports) == 1
    assert status.profile_name == "solo"


# ── drift detected ──────────────────────────────────────────────────────


def test_build_status_drifted_reports_drift() -> None:
    gov = load_governance_config(_solo_config())
    declared = _rules_solo()
    live = declared.with_overrides(require_strict_status=True)  # drift
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=live,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    assert status.has_drift is True
    assert status.reports[0].drifted_entries[0].field_name == "require_strict_status"


# ── unprotected branch ──────────────────────────────────────────────────


def test_build_status_unprotected_branch_reports_drift() -> None:
    gov = load_governance_config(_solo_config())
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=None,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    assert status.has_drift is True
    assert status.reports[0].live_unprotected is True


# ── per-branch error handling ───────────────────────────────────────────


def test_build_status_collects_errors_per_branch() -> None:
    from shipyard.governance.github import GovernanceApiError
    gov = load_governance_config(_solo_config())

    def fake_get(repo, branch, gh_command="gh"):
        if branch == "broken":
            raise GovernanceApiError("permission denied on broken")
        return _rules_solo()

    with patch(
        "shipyard.governance.status.get_branch_protection",
        side_effect=fake_get,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main", "broken"),
        )
    # main is reported, broken is in errors
    assert len(status.reports) == 1
    assert status.reports[0].branch == "main"
    assert status.has_errors is True
    assert any("permission denied" in e for e in status.errors)


# ── format_status_text ──────────────────────────────────────────────────


def test_format_aligned_status_shows_check_mark() -> None:
    gov = load_governance_config(_solo_config())
    declared = _rules_solo()
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=declared,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    text = format_status_text(status)
    assert "me/r" in text
    assert "Profile: solo" in text
    assert "aligned with profile" in text


def test_format_drifted_status_shows_fix_hint() -> None:
    gov = load_governance_config(_solo_config())
    declared = _rules_solo()
    live = declared.with_overrides(enforce_admins=True)
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=live,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    text = format_status_text(status)
    assert "drifted" in text
    assert "enforce_admins" in text
    assert "shipyard governance apply" in text


def test_format_unprotected_status_is_loud() -> None:
    gov = load_governance_config(_solo_config())
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=None,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    text = format_status_text(status)
    assert "UNPROTECTED" in text


def test_format_deviated_but_aligned_shows_info_row() -> None:
    """An intentional override shows up as an info line, not a drift error."""
    cfg = Config(data={
        "project": {"profile": "solo"},
        "governance": {"required_status_checks": ["mac"]},
        "branch_protection": {"main": {"require_review_count": 2}},
    })
    gov = load_governance_config(cfg)
    from shipyard.governance.config import resolve_branch_rules
    declared = resolve_branch_rules(gov, "main")
    with patch(
        "shipyard.governance.status.get_branch_protection",
        return_value=declared,
    ):
        status = build_status(
            repo=RepoRef("me", "r"),
            governance=gov,
            branches=("main",),
        )
    text = format_status_text(status)
    assert "deviated from profile" in text
    assert "require_review_count" in text
