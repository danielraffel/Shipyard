"""Layer 1: profile definition unit tests.

Every entry in the solo and multi tables from the planning doc has
an assertion here. If a knob is added to BranchProtectionRules, its
expected value for both profiles MUST be asserted in this file — a
small lint script enforces that in CI.
"""

from __future__ import annotations

import pytest

from shipyard.governance.profiles import (
    BranchProtectionRules,
    known_profile_names,
    multi_profile,
    profile_for_name,
    solo_profile,
)

# ── solo profile hard-coded defaults ────────────────────────────────────


def test_solo_profile_defaults() -> None:
    p = solo_profile()
    rules = p.branch_protection
    assert p.name == "solo"
    assert rules.require_pr is True
    assert rules.require_status_checks == ()
    # The defining solo choice — see planning doc Part 12:
    # strict=false is what avoids the rebase tax on single-maintainer
    # projects. Test this specifically because it's the most-asked
    # about knob and the one PR #106 proved matters.
    assert rules.require_strict_status is False
    assert rules.require_review_count == 0
    assert rules.enforce_admins is False
    assert rules.dismiss_stale_reviews is False
    assert rules.require_code_owner_reviews is False
    assert rules.allow_force_push is False
    assert rules.allow_deletions is False
    assert rules.require_linear_history is False
    # Off for solo by design — conversation resolution has no value
    # on a single-maintainer project and is pure self-friction. Multi
    # profile keeps it on.
    assert rules.required_conversation_resolution is False


def test_solo_profile_applies_required_status_checks() -> None:
    p = solo_profile(required_status_checks=("macOS", "Linux", "Windows"))
    assert p.branch_protection.require_status_checks == ("macOS", "Linux", "Windows")


# ── multi profile hard-coded defaults ───────────────────────────────────


def test_multi_profile_defaults() -> None:
    p = multi_profile()
    rules = p.branch_protection
    assert p.name == "multi"
    assert rules.require_pr is True
    assert rules.require_status_checks == ()
    assert rules.require_strict_status is True
    assert rules.require_review_count == 1
    assert rules.enforce_admins is True
    assert rules.dismiss_stale_reviews is True
    assert rules.require_code_owner_reviews is False
    assert rules.allow_force_push is False
    assert rules.allow_deletions is False
    assert rules.require_linear_history is False
    assert rules.required_conversation_resolution is True


def test_multi_profile_applies_required_status_checks() -> None:
    p = multi_profile(required_status_checks=("x", "y"))
    assert p.branch_protection.require_status_checks == ("x", "y")


# ── solo vs multi must differ in the right places ──────────────────────


def test_solo_and_multi_differ_on_key_knobs() -> None:
    """Catch a bug where the two profiles accidentally collapse to the same defaults."""
    s = solo_profile().branch_protection
    m = multi_profile().branch_protection
    # These are the process gates that separate "single maintainer"
    # from "coordinating with other humans" — if any of them stop
    # differing, either a profile is wrong or the distinction has
    # been erased.
    assert s.require_strict_status != m.require_strict_status
    assert s.require_review_count != m.require_review_count
    assert s.enforce_admins != m.enforce_admins
    assert s.dismiss_stale_reviews != m.dismiss_stale_reviews


def test_solo_and_multi_agree_on_always_off_knobs() -> None:
    """Some knobs are never on for either profile (force-push, deletions)."""
    s = solo_profile().branch_protection
    m = multi_profile().branch_protection
    assert s.allow_force_push is False and m.allow_force_push is False
    assert s.allow_deletions is False and m.allow_deletions is False
    assert s.require_linear_history is False and m.require_linear_history is False


# ── profile_for_name lookup + custom profile ────────────────────────────


def test_profile_for_name_solo() -> None:
    p = profile_for_name("solo", required_status_checks=("a",))
    assert p.name == "solo"
    assert p.branch_protection.require_status_checks == ("a",)


def test_profile_for_name_multi() -> None:
    p = profile_for_name("multi")
    assert p.name == "multi"


def test_profile_for_name_custom_is_empty() -> None:
    """custom returns a blank BranchProtectionRules with default field values."""
    p = profile_for_name("custom")
    assert p.name == "custom"
    # The custom profile's default rules match BranchProtectionRules()
    # default construction — every knob at its most restrictive sane
    # default, caller is expected to override.
    default = BranchProtectionRules()
    assert p.branch_protection == default


def test_profile_for_name_unknown_raises() -> None:
    with pytest.raises(ValueError, match="Unknown governance profile"):
        profile_for_name("enterprise")


def test_known_profile_names_has_all_three() -> None:
    names = known_profile_names()
    assert set(names) == {"solo", "multi", "custom"}


# ── with_overrides preserves frozen behavior ────────────────────────────


def test_with_overrides_returns_new_instance() -> None:
    base = BranchProtectionRules()
    new = base.with_overrides(require_review_count=5)
    assert new.require_review_count == 5
    assert base.require_review_count == 0
    # Frozen dataclass — the original is unchanged
    assert base != new
