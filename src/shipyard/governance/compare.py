"""Drift detection: compare declared config, profile defaults, and live GitHub state.

`compute_drift` returns a `DriftReport` with one entry per governance
knob, each carrying four values:

- the profile's default (what the preset says)
- the declared value from `.shipyard/config.toml` (what the project wants)
- the live GitHub value (what GitHub actually has)
- a `status` field that categorises the entry:
  - "aligned"  — profile == declared == live
  - "deviated" — declared != profile (intentional override)
  - "drifted"  — live != declared (needs `apply` to fix)
  - "both"     — both deviated AND drifted

The rollup is purely a read-model; it never mutates state. Callers
that want to fix drift pass the report to `apply.build_apply_plan`.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from shipyard.governance.profiles import BranchProtectionRules


class DriftStatus(str, Enum):
    ALIGNED = "aligned"
    DEVIATED = "deviated"
    DRIFTED = "drifted"
    BOTH = "both"
    UNPROTECTED = "unprotected"  # live has no protection at all


@dataclass(frozen=True)
class DriftEntry:
    """One row of the drift matrix."""

    field_name: str
    profile_value: Any
    declared_value: Any
    live_value: Any
    status: DriftStatus

    @property
    def needs_apply(self) -> bool:
        """True if running `apply` would change the live state."""
        return self.status in (DriftStatus.DRIFTED, DriftStatus.BOTH, DriftStatus.UNPROTECTED)


@dataclass(frozen=True)
class DriftReport:
    """Aggregate drift across one branch's worth of rules."""

    branch: str
    entries: tuple[DriftEntry, ...] = field(default_factory=tuple)
    live_unprotected: bool = False

    @property
    def has_drift(self) -> bool:
        return self.live_unprotected or any(e.needs_apply for e in self.entries)

    @property
    def drifted_entries(self) -> tuple[DriftEntry, ...]:
        return tuple(e for e in self.entries if e.needs_apply)

    @property
    def deviated_entries(self) -> tuple[DriftEntry, ...]:
        return tuple(
            e for e in self.entries
            if e.status in (DriftStatus.DEVIATED, DriftStatus.BOTH)
        )


# Fields that contribute to the drift matrix. Must match the
# BranchProtectionRules dataclass field names exactly.
_GOVERNANCE_FIELDS: tuple[str, ...] = (
    "require_pr",
    "require_status_checks",
    "require_strict_status",
    "require_review_count",
    "enforce_admins",
    "dismiss_stale_reviews",
    "require_code_owner_reviews",
    "allow_force_push",
    "allow_deletions",
    "require_linear_history",
    "required_conversation_resolution",
)


def compute_drift(
    *,
    branch: str,
    profile_rules: BranchProtectionRules,
    declared_rules: BranchProtectionRules,
    live_rules: BranchProtectionRules | None,
) -> DriftReport:
    """Compare profile vs declared vs live and return a DriftReport.

    A `live_rules` of None means the branch has no protection at
    all — every field will be flagged as needing apply, and the
    report's `live_unprotected` flag is set for clearer UI output.
    """
    if live_rules is None:
        return _unprotected_report(
            branch=branch,
            profile_rules=profile_rules,
            declared_rules=declared_rules,
        )

    entries: list[DriftEntry] = []
    for field_name in _GOVERNANCE_FIELDS:
        profile_value = _normalize(getattr(profile_rules, field_name))
        declared_value = _normalize(getattr(declared_rules, field_name))
        live_value = _normalize(getattr(live_rules, field_name))

        deviated = declared_value != profile_value
        drifted = live_value != declared_value
        if deviated and drifted:
            status = DriftStatus.BOTH
        elif deviated:
            status = DriftStatus.DEVIATED
        elif drifted:
            status = DriftStatus.DRIFTED
        else:
            status = DriftStatus.ALIGNED

        entries.append(
            DriftEntry(
                field_name=field_name,
                profile_value=profile_value,
                declared_value=declared_value,
                live_value=live_value,
                status=status,
            )
        )

    return DriftReport(branch=branch, entries=tuple(entries))


def _unprotected_report(
    *,
    branch: str,
    profile_rules: BranchProtectionRules,
    declared_rules: BranchProtectionRules,
) -> DriftReport:
    """Build a report for a branch with no protection set at all."""
    entries: list[DriftEntry] = []
    for field_name in _GOVERNANCE_FIELDS:
        profile_value = _normalize(getattr(profile_rules, field_name))
        declared_value = _normalize(getattr(declared_rules, field_name))
        entries.append(
            DriftEntry(
                field_name=field_name,
                profile_value=profile_value,
                declared_value=declared_value,
                live_value=None,
                status=DriftStatus.UNPROTECTED,
            )
        )
    return DriftReport(branch=branch, entries=tuple(entries), live_unprotected=True)


def _normalize(value: Any) -> Any:
    """Normalize values so equality comparison works across tuple/list/set boundaries.

    GitHub returns status check contexts as a list; Shipyard stores
    them as a tuple; ruamel or tomllib might return them as lists.
    Normalising to a sorted tuple means comparison is order-insensitive
    and type-stable.
    """
    if isinstance(value, (list, tuple, set, frozenset)):
        return tuple(sorted(str(v) for v in value))
    return value
