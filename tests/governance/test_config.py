"""Layer 2: governance config parsing and branch-rule resolution."""

from __future__ import annotations

import pytest

from shipyard.core.config import Config
from shipyard.governance.config import (
    load_governance_config,
    resolve_branch_rules,
)


def _config(data: dict) -> Config:
    return Config(data=data)


# ── load_governance_config ──────────────────────────────────────────────


def test_load_defaults_to_solo_with_no_section() -> None:
    gov = load_governance_config(_config({}))
    assert gov.profile.name == "solo"
    assert gov.required_status_checks == ()
    assert gov.branch_overrides == {}


def test_load_solo_with_required_status_checks() -> None:
    gov = load_governance_config(_config({
        "project": {"profile": "solo"},
        "governance": {"required_status_checks": ["mac", "linux", "win"]},
    }))
    assert gov.profile.name == "solo"
    assert gov.required_status_checks == ("mac", "linux", "win")
    assert gov.profile.branch_protection.require_status_checks == ("mac", "linux", "win")


def test_load_multi_profile() -> None:
    gov = load_governance_config(_config({
        "project": {"profile": "multi"},
    }))
    assert gov.profile.name == "multi"
    assert gov.profile.branch_protection.require_strict_status is True


def test_load_backwards_compat_with_merge_require_platforms() -> None:
    """When only `[merge].require_platforms` is set, treat it as required checks."""
    gov = load_governance_config(_config({
        "merge": {"require_platforms": ["macOS", "Linux"]},
    }))
    assert gov.required_status_checks == ("macOS", "Linux")


def test_load_governance_required_status_checks_wins_over_merge() -> None:
    gov = load_governance_config(_config({
        "merge": {"require_platforms": ["old"]},
        "governance": {"required_status_checks": ["new"]},
    }))
    assert gov.required_status_checks == ("new",)


def test_load_unknown_profile_raises() -> None:
    with pytest.raises(ValueError, match="Unknown governance profile"):
        load_governance_config(_config({"project": {"profile": "weird"}}))


def test_load_branch_overrides() -> None:
    gov = load_governance_config(_config({
        "branch_protection": {
            "main": {"require_review_count": 2, "enforce_admins": True},
            "develop/**": {"extends": "main", "require_review_count": 0},
        },
    }))
    assert "main" in gov.branch_overrides
    assert gov.branch_overrides["main"]["require_review_count"] == 2
    assert gov.branch_overrides["develop/**"]["extends"] == "main"


# ── resolve_branch_rules ────────────────────────────────────────────────


def test_resolve_rules_for_branch_with_no_overrides() -> None:
    """A branch not matching any glob gets the profile default as-is."""
    gov = load_governance_config(_config({"project": {"profile": "solo"}}))
    rules = resolve_branch_rules(gov, "main")
    assert rules.require_strict_status is False  # solo default
    assert rules.require_review_count == 0


def test_resolve_rules_applies_override_on_top_of_profile() -> None:
    gov = load_governance_config(_config({
        "project": {"profile": "solo"},
        "branch_protection": {
            "main": {"require_review_count": 2},
        },
    }))
    rules = resolve_branch_rules(gov, "main")
    assert rules.require_review_count == 2  # override
    assert rules.require_strict_status is False  # still solo default


def test_resolve_rules_applies_glob_match() -> None:
    gov = load_governance_config(_config({
        "project": {"profile": "solo"},
        "branch_protection": {
            "develop/**": {"require_review_count": 1},
        },
    }))
    dev_rules = resolve_branch_rules(gov, "develop/feature-x")
    assert dev_rules.require_review_count == 1

    main_rules = resolve_branch_rules(gov, "main")
    assert main_rules.require_review_count == 0  # unmatched


def test_resolve_rules_extends_chains_parent_overrides() -> None:
    gov = load_governance_config(_config({
        "project": {"profile": "solo"},
        "branch_protection": {
            "main": {"enforce_admins": True},
            "develop/**": {"extends": "main", "require_review_count": 1},
        },
    }))
    rules = resolve_branch_rules(gov, "develop/auth")
    # Inherited from main
    assert rules.enforce_admins is True
    # Declared directly
    assert rules.require_review_count == 1


def test_resolve_rules_rejects_unknown_field() -> None:
    gov = load_governance_config(_config({
        "branch_protection": {
            "main": {"typo_field": True},
        },
    }))
    with pytest.raises(ValueError, match="Unknown branch_protection field 'typo_field'"):
        resolve_branch_rules(gov, "main")


def test_resolve_rules_converts_list_to_tuple_for_status_checks() -> None:
    gov = load_governance_config(_config({
        "branch_protection": {
            "main": {"require_status_checks": ["mac", "linux"]},
        },
    }))
    rules = resolve_branch_rules(gov, "main")
    assert rules.require_status_checks == ("mac", "linux")


def test_resolve_rules_later_glob_wins_over_earlier() -> None:
    """When two globs both match, the later one overrides the earlier one."""
    gov = load_governance_config(_config({
        "branch_protection": {
            "main": {"require_review_count": 1},
            "*": {"require_review_count": 3},  # also matches main
        },
    }))
    rules = resolve_branch_rules(gov, "main")
    assert rules.require_review_count == 3  # later wins
