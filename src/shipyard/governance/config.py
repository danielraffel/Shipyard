"""Read governance config from `.shipyard/config.toml`.

Config schema::

    [project]
    profile = "solo"   # or "multi" or "custom"

    [governance]
    # Optional — default derived from project.profile
    required_status_checks = ["macOS (ARM64)", "Linux (x64)", "Windows (x64)"]

    [branch_protection."main"]
    # Explicit overrides on top of the profile default
    require_review_count = 1     # override just this one knob
    enforce_admins       = true

    [branch_protection."develop/**"]
    extends = "main"             # inherit main's overrides
    require_review_count = 0     # but drop the review count

The resolution order for a given branch is:

1. Start with the profile's default rules
2. Apply any `[branch_protection."<glob>"]` overrides for matching globs
3. If a glob has `extends = "<other>"`, apply the base glob's overrides first
4. Last override wins

This module returns a `GovernanceConfig` object plus a helper to
resolve the effective rules for a specific branch name. The
GitHub-side apply logic (in `apply.py`) takes that resolved object
and diffs it against live state.
"""

from __future__ import annotations

import fnmatch
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

from shipyard.governance.profiles import (
    BranchProtectionRules,
    Profile,
    profile_for_name,
)

if TYPE_CHECKING:
    from shipyard.core.config import Config


@dataclass(frozen=True)
class GovernanceConfig:
    """Parsed `[governance]` + `[project.profile]` + `[branch_protection.*]` state.

    The `branch_overrides` dict is keyed by glob pattern and stores a
    raw dict of field-name → value overrides (not a full rules
    object). Glob resolution happens in `resolve_branch_rules` so the
    GovernanceConfig stays a faithful representation of what the
    user wrote.
    """

    profile: Profile
    required_status_checks: tuple[str, ...]
    branch_overrides: dict[str, dict[str, Any]] = field(default_factory=dict)


def load_governance_config(config: Config) -> GovernanceConfig:
    """Parse the `[project]`, `[governance]`, and `[branch_protection.*]` sections.

    Defaults when not declared:
    - profile → "solo" (opinionated default — most Shipyard projects
      are solo, and the solo profile is the least surprising starting
      point for anyone who hasn't yet declared one)
    - required_status_checks → derived from `[governance]` if set,
      otherwise empty
    - branch_overrides → empty dict

    Raises ValueError if `profile` names an unknown profile.
    """
    profile_name = str(config.get("project.profile", "solo"))

    # Required status checks can be declared in `[governance]` or
    # inherited from the old `[merge] require_platforms` list for
    # backwards compatibility with existing Shipyard projects that
    # predate the governance section.
    declared_checks = config.get("governance.required_status_checks")
    if declared_checks is None:
        declared_checks = config.get("merge.require_platforms", [])
    if not isinstance(declared_checks, list):
        declared_checks = []
    required_status_checks = tuple(str(c) for c in declared_checks)

    profile = profile_for_name(
        profile_name, required_status_checks=required_status_checks,
    )

    branch_overrides: dict[str, dict[str, Any]] = {}
    raw_bp = config.get("branch_protection")
    if isinstance(raw_bp, dict):
        for glob, overrides in raw_bp.items():
            if isinstance(overrides, dict):
                branch_overrides[str(glob)] = dict(overrides)

    return GovernanceConfig(
        profile=profile,
        required_status_checks=required_status_checks,
        branch_overrides=branch_overrides,
    )


def resolve_branch_rules(
    governance: GovernanceConfig,
    branch_name: str,
) -> BranchProtectionRules:
    """Compute the effective rules for a branch by merging overrides.

    Walks every `[branch_protection."<glob>"]` in the config in order
    of declaration, applies the ones that match `branch_name` on top
    of the profile default, and returns the resulting frozen rules.

    `extends = "<other>"` pulls in the base glob's overrides first,
    so a chain like `main` → `develop/**` → `release/**` can inherit
    cleanly without duplicating every field.
    """
    rules = governance.profile.branch_protection

    overrides = _resolve_overrides_for(branch_name, governance.branch_overrides)
    if overrides:
        # Only pass recognised fields through to with_overrides; any
        # unknown key is a config typo and should not silently
        # disappear. Raising here surfaces the typo at load time.
        known_fields = set(BranchProtectionRules.__dataclass_fields__.keys())
        for key in overrides:
            if key == "extends":
                continue
            if key not in known_fields:
                raise ValueError(
                    f"Unknown branch_protection field '{key}' for "
                    f"branch '{branch_name}'. Known fields: "
                    f"{', '.join(sorted(known_fields))}"
                )
        clean = {k: v for k, v in overrides.items() if k != "extends"}
        # Convert list→tuple for status checks so the dataclass stays hashable.
        if "require_status_checks" in clean and isinstance(
            clean["require_status_checks"], list,
        ):
            clean["require_status_checks"] = tuple(
                str(x) for x in clean["require_status_checks"]
            )
        rules = rules.with_overrides(**clean)

    return rules


def _resolve_overrides_for(
    branch_name: str,
    branch_overrides: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    """Walk the branch_overrides dict, apply extends, merge matching globs.

    A later matching glob wins over an earlier one.
    """
    merged: dict[str, Any] = {}
    for glob, overrides in branch_overrides.items():
        if not fnmatch.fnmatchcase(branch_name, glob):
            continue
        # Apply parent's overrides first if `extends` is set
        parent_name = overrides.get("extends")
        if parent_name and parent_name in branch_overrides:
            parent = branch_overrides[parent_name]
            for k, v in parent.items():
                if k != "extends":
                    merged[k] = v
        for k, v in overrides.items():
            if k != "extends":
                merged[k] = v
    return merged
