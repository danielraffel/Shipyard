"""Drift computation tests — compare profile vs declared vs live state."""

from __future__ import annotations

from shipyard.governance.compare import DriftStatus, compute_drift
from shipyard.governance.profiles import (
    BranchProtectionRules,
    multi_profile,
    solo_profile,
)

# ── aligned: profile == declared == live ────────────────────────────────


def test_fully_aligned_report_has_no_drift() -> None:
    p = solo_profile()
    report = compute_drift(
        branch="main",
        profile_rules=p.branch_protection,
        declared_rules=p.branch_protection,
        live_rules=p.branch_protection,
    )
    assert report.has_drift is False
    assert report.drifted_entries == ()
    assert report.deviated_entries == ()
    assert all(e.status == DriftStatus.ALIGNED for e in report.entries)


# ── drifted: declared matches profile but live differs ─────────────────


def test_live_drifted_from_declared_is_flagged() -> None:
    p = solo_profile()
    live = p.branch_protection.with_overrides(enforce_admins=True)  # drift
    report = compute_drift(
        branch="main",
        profile_rules=p.branch_protection,
        declared_rules=p.branch_protection,
        live_rules=live,
    )
    assert report.has_drift is True
    drifted = report.drifted_entries
    assert len(drifted) == 1
    assert drifted[0].field_name == "enforce_admins"
    assert drifted[0].status == DriftStatus.DRIFTED


# ── deviated: declared differs from profile but live matches declared ──


def test_declared_deviated_from_profile_but_live_matches() -> None:
    """An intentional override is reported as deviated, not drifted."""
    p = solo_profile()
    declared = p.branch_protection.with_overrides(require_review_count=2)
    live = declared  # live has been applied
    report = compute_drift(
        branch="main",
        profile_rules=p.branch_protection,
        declared_rules=declared,
        live_rules=live,
    )
    # Deviated from profile but no drift to fix
    assert report.has_drift is False
    deviated = report.deviated_entries
    assert len(deviated) == 1
    assert deviated[0].field_name == "require_review_count"
    assert deviated[0].status == DriftStatus.DEVIATED


# ── both: declared differs from profile AND live differs from declared ─


def test_both_deviated_and_drifted() -> None:
    p = solo_profile()
    declared = p.branch_protection.with_overrides(require_review_count=2)
    live = p.branch_protection.with_overrides(require_review_count=5)
    report = compute_drift(
        branch="main",
        profile_rules=p.branch_protection,
        declared_rules=declared,
        live_rules=live,
    )
    assert report.has_drift is True
    both = [e for e in report.entries if e.status == DriftStatus.BOTH]
    assert len(both) == 1
    assert both[0].field_name == "require_review_count"


# ── live_unprotected: branch has no protection at all ──────────────────


def test_unprotected_branch_is_flagged() -> None:
    p = solo_profile()
    report = compute_drift(
        branch="main",
        profile_rules=p.branch_protection,
        declared_rules=p.branch_protection,
        live_rules=None,
    )
    assert report.has_drift is True
    assert report.live_unprotected is True
    assert all(e.status == DriftStatus.UNPROTECTED for e in report.entries)
    assert all(e.needs_apply for e in report.entries)


# ── status check list ordering is normalized ──────────────────────────


def test_status_check_order_is_ignored() -> None:
    """Two lists with the same contents in different order compare equal."""
    declared = BranchProtectionRules(
        require_status_checks=("a", "b", "c"),
    )
    live = BranchProtectionRules(
        require_status_checks=("c", "a", "b"),
    )
    report = compute_drift(
        branch="main",
        profile_rules=declared,
        declared_rules=declared,
        live_rules=live,
    )
    # Status checks should compare as equal (order-insensitive)
    check_entry = next(
        e for e in report.entries if e.field_name == "require_status_checks"
    )
    assert check_entry.status == DriftStatus.ALIGNED


def test_status_checks_differ_when_contents_differ() -> None:
    declared = BranchProtectionRules(
        require_status_checks=("a", "b"),
    )
    live = BranchProtectionRules(
        require_status_checks=("a", "b", "c"),
    )
    report = compute_drift(
        branch="main",
        profile_rules=declared,
        declared_rules=declared,
        live_rules=live,
    )
    check_entry = next(
        e for e in report.entries if e.field_name == "require_status_checks"
    )
    assert check_entry.status == DriftStatus.DRIFTED


# ── multi profile drift flows ──────────────────────────────────────────


def test_multi_profile_applied_solo_live_reports_drift_on_gates() -> None:
    """A repo declared as multi but with solo-shaped live state has lots of drift."""
    m = multi_profile()
    solo_shaped_live = BranchProtectionRules(
        require_pr=True,
        require_strict_status=False,
        require_review_count=0,
        enforce_admins=False,
        dismiss_stale_reviews=False,
    )
    report = compute_drift(
        branch="main",
        profile_rules=m.branch_protection,
        declared_rules=m.branch_protection,
        live_rules=solo_shaped_live,
    )
    drifted_fields = {e.field_name for e in report.drifted_entries}
    assert "require_strict_status" in drifted_fields
    assert "require_review_count" in drifted_fields
    assert "enforce_admins" in drifted_fields
    assert "dismiss_stale_reviews" in drifted_fields
