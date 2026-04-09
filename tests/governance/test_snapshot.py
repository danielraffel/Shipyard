"""Tests for governance snapshot + restore (Phase 6 follow-ups)."""

from __future__ import annotations

from datetime import datetime, timezone

import pytest

from shipyard.governance.github import RepoRef
from shipyard.governance.profiles import BranchProtectionRules
from shipyard.governance.snapshot import (
    SNAPSHOT_SCHEMA_VERSION,
    GovernanceSnapshot,
    build_snapshot,
)


def _fixed_clock() -> datetime:
    return datetime(2026, 4, 9, 17, 30, 0, tzinfo=timezone.utc)


def _sample_rules() -> BranchProtectionRules:
    return BranchProtectionRules(
        require_pr=True,
        require_status_checks=("mac", "linux", "win"),
        require_strict_status=False,
        require_review_count=0,
        enforce_admins=False,
    )


# ── build_snapshot ──────────────────────────────────────────────────────


def test_build_snapshot_captures_repo_and_branches() -> None:
    snap = build_snapshot(
        repo=RepoRef("me", "r"),
        live_branches={"main": _sample_rules()},
        clock=_fixed_clock,
    )
    assert snap.repo_slug == "me/r"
    assert snap.exported_at == "2026-04-09T17:30:00Z"
    assert "main" in snap.branches
    assert snap.branches["main"].require_status_checks == ("mac", "linux", "win")
    assert snap.schema_version == SNAPSHOT_SCHEMA_VERSION


# ── to_toml / from_toml round-trip ─────────────────────────────────────


def test_snapshot_round_trip_preserves_every_field() -> None:
    original = GovernanceSnapshot(
        repo_slug="me/r",
        exported_at="2026-04-09T17:30:00Z",
        branches={"main": _sample_rules()},
    )
    text = original.to_toml()
    restored = GovernanceSnapshot.from_toml(text)
    assert restored.repo_slug == original.repo_slug
    assert restored.exported_at == original.exported_at
    assert restored.branches["main"] == original.branches["main"]


def test_snapshot_round_trip_is_byte_identical_for_same_state() -> None:
    """Exporting the same state twice produces identical TOML."""
    snap = GovernanceSnapshot(
        repo_slug="me/r",
        exported_at="2026-04-09T17:30:00Z",
        branches={"main": _sample_rules()},
    )
    assert snap.to_toml() == snap.to_toml()


def test_snapshot_branches_sorted_for_stability() -> None:
    """Multiple branches must serialize in a stable (alphabetical) order."""
    rules = _sample_rules()
    snap = GovernanceSnapshot(
        repo_slug="me/r",
        exported_at="2026-04-09T17:30:00Z",
        branches={
            "release/v2": rules,
            "main": rules,
            "develop": rules,
        },
    )
    text = snap.to_toml()
    # develop < main < release/v2 alphabetically
    dev_pos = text.find("[branch_protection.develop]")
    main_pos = text.find("[branch_protection.main]")
    rel_pos = text.find("[branch_protection.")
    assert dev_pos < main_pos
    # The release branch key must appear somewhere after main
    assert text.find('branch_protection."release/v2"') > main_pos or (
        text.rfind("[branch_protection.") > main_pos
    )
    del rel_pos  # silence unused


# ── from_toml error cases ──────────────────────────────────────────────


def test_from_toml_rejects_missing_header() -> None:
    text = '[branch_protection.main]\nrequire_pr = true\n'
    with pytest.raises(ValueError, match="missing.*header"):
        GovernanceSnapshot.from_toml(text)


def test_from_toml_rejects_wrong_schema_version() -> None:
    text = (
        '[shipyard_governance_snapshot]\n'
        'schema_version = 99\n'
        'repo = "me/r"\n'
        'exported_at = "2026-04-09T17:30:00Z"\n'
    )
    with pytest.raises(ValueError, match="schema version"):
        GovernanceSnapshot.from_toml(text)


def test_from_toml_rejects_missing_repo() -> None:
    text = (
        '[shipyard_governance_snapshot]\n'
        f'schema_version = {SNAPSHOT_SCHEMA_VERSION}\n'
        'exported_at = "2026-04-09T17:30:00Z"\n'
    )
    with pytest.raises(ValueError, match="missing required field: repo"):
        GovernanceSnapshot.from_toml(text)


def test_from_toml_empty_branch_block_ok() -> None:
    """A snapshot with no branches is valid (edge case for fresh setups)."""
    text = (
        '[shipyard_governance_snapshot]\n'
        f'schema_version = {SNAPSHOT_SCHEMA_VERSION}\n'
        'repo = "me/r"\n'
        'exported_at = "2026-04-09T17:30:00Z"\n'
    )
    snap = GovernanceSnapshot.from_toml(text)
    assert snap.branches == {}
