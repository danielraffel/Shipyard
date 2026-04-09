"""Governance profile definitions: solo, multi, custom.

A profile is a named bundle of governance defaults. Picking one sets
the floor for every knob in `.shipyard/config.toml` — individual
knobs can still be overridden explicitly.

The three profiles map to the three project shapes Shipyard targets:

- **solo**: single maintainer, no third-party committers. Optimized
  for speed and no self-friction. `strict` is off because there is
  nobody else to coordinate with; required reviews is zero because
  you cannot review your own PR; admins are not enforced because
  you ARE the admin and need a path for the 3am hotfix.

- **multi**: multiple maintainers, third-party PRs accepted. Every
  process gate is on. Strict required checks, one required review,
  enforce on admins, dismiss stale reviews, manual release approval.

- **custom**: explicit per-knob; no preset. Used when neither solo
  nor multi fits. Also what you get implicitly if you set a profile
  and then override specific knobs.

The profile defaults are hard-coded here rather than in a TOML file
because they are part of Shipyard's API surface — a future Shipyard
release can evolve the presets (with a migration warning) but no
project should need to write its own profile table. Custom projects
declare explicit overrides on top of a base profile.
"""

from __future__ import annotations

from dataclasses import dataclass, field, replace
from typing import Literal

ProfileName = Literal["solo", "multi", "custom"]

_KNOWN_PROFILES: tuple[ProfileName, ...] = ("solo", "multi", "custom")


@dataclass(frozen=True)
class BranchProtectionRules:
    """The per-branch knobs Shipyard governs for branch protection.

    Every field maps 1:1 to a GitHub branch protection setting.
    Keeping this flat and frozen makes the profile-vs-declared-vs-live
    drift computation a simple field-by-field comparison.

    Fields correspond to the classic branch protection API:
    https://docs.github.com/en/rest/branches/branch-protection

    Note: This dataclass intentionally covers only branch protection
    for v0.1.4. Tag protection, default workflow permissions, and
    rulesets are tracked as separate concerns; they'll land in
    follow-up PRs with their own dataclasses.
    """

    require_pr: bool = True
    require_status_checks: tuple[str, ...] = ()
    require_strict_status: bool = False
    require_review_count: int = 0
    enforce_admins: bool = False
    dismiss_stale_reviews: bool = False
    require_code_owner_reviews: bool = False
    allow_force_push: bool = False
    allow_deletions: bool = False
    require_linear_history: bool = False
    # Off by default — forcing conversation resolution on a solo
    # project is self-friction (nobody else is leaving comments), and
    # the planning doc's solo/multi table intentionally doesn't list
    # this knob as one of the distinguishing defaults. Multi projects
    # that want it can override to True explicitly.
    required_conversation_resolution: bool = False

    def with_overrides(self, **overrides: object) -> BranchProtectionRules:
        """Return a copy with the given fields replaced.

        Used when merging a profile default with per-branch overrides
        declared in `.shipyard/config.toml`.
        """
        return replace(self, **overrides)  # type: ignore[arg-type]


@dataclass(frozen=True)
class Profile:
    """A named bundle of governance defaults.

    For v0.1.4 a profile only sets branch protection defaults; as
    more knobs are added (tag protection, workflow permissions,
    etc.) they will be added as additional fields here.
    """

    name: ProfileName
    branch_protection: BranchProtectionRules = field(default_factory=BranchProtectionRules)


def solo_profile(
    *,
    required_status_checks: tuple[str, ...] = (),
) -> Profile:
    """Return the `solo` profile with the given required status checks.

    The required status check names are project-specific (Pulp uses
    macOS/Linux/Windows) so they're parameterized. Everything else is
    fixed.
    """
    return Profile(
        name="solo",
        branch_protection=BranchProtectionRules(
            require_pr=True,
            require_status_checks=required_status_checks,
            # strict=false is THE defining solo choice. Without it,
            # every PR falling behind main needs a rebase — pure
            # busywork when there's no second contributor to
            # coordinate with.
            require_strict_status=False,
            require_review_count=0,
            enforce_admins=False,
            dismiss_stale_reviews=False,
            require_code_owner_reviews=False,
            allow_force_push=False,
            allow_deletions=False,
            require_linear_history=False,
            required_conversation_resolution=False,
        ),
    )


def multi_profile(
    *,
    required_status_checks: tuple[str, ...] = (),
) -> Profile:
    """Return the `multi` profile with the given required status checks.

    The differences from `solo` are exactly the process gates that
    exist to coordinate multiple humans: strict required checks,
    required reviews, enforce on admins, dismiss stale reviews.
    """
    return Profile(
        name="multi",
        branch_protection=BranchProtectionRules(
            require_pr=True,
            require_status_checks=required_status_checks,
            require_strict_status=True,
            require_review_count=1,
            enforce_admins=True,
            dismiss_stale_reviews=True,
            require_code_owner_reviews=False,
            allow_force_push=False,
            allow_deletions=False,
            require_linear_history=False,
            required_conversation_resolution=True,
        ),
    )


def profile_for_name(
    name: str,
    *,
    required_status_checks: tuple[str, ...] = (),
) -> Profile:
    """Look up a profile by name; raise ValueError on unknown names.

    `custom` is accepted but returns an empty-rules Profile — callers
    are expected to merge explicit overrides on top of it.
    """
    if name == "solo":
        return solo_profile(required_status_checks=required_status_checks)
    if name == "multi":
        return multi_profile(required_status_checks=required_status_checks)
    if name == "custom":
        return Profile(
            name="custom",
            branch_protection=BranchProtectionRules(
                require_status_checks=required_status_checks,
            ),
        )
    raise ValueError(
        f"Unknown governance profile '{name}'. "
        f"Expected one of: {', '.join(_KNOWN_PROFILES)}"
    )


def known_profile_names() -> tuple[ProfileName, ...]:
    """Public list of accepted profile names (for error messages / CLI help)."""
    return _KNOWN_PROFILES
