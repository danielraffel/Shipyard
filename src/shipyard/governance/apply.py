"""Idempotent governance apply.

`build_apply_plan` computes the set of PUT calls that would be
needed to bring a branch's live state in line with its declared
state. `execute_apply_plan` actually issues them. The two-step API
exists so `governance diff` and `governance apply --dry-run` can
share exactly the same planning code without duplicating the PUT
logic.

Idempotency contract: calling `execute_apply_plan` on a plan
produced from up-to-date state issues zero PUT calls. Calling it
twice in a row (without drift in between) is a no-op.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from enum import Enum
from typing import TYPE_CHECKING

from shipyard.governance.github import (
    GovernanceApiError,
    put_branch_protection,
)

if TYPE_CHECKING:
    from shipyard.governance.compare import DriftReport
    from shipyard.governance.github import RepoRef
    from shipyard.governance.profiles import BranchProtectionRules


class ApplyAction(str, Enum):
    NOOP = "noop"
    UPDATE = "update"  # live protection exists, needs changes
    CREATE = "create"  # live protection missing, needs creation


@dataclass(frozen=True)
class ApplyPlan:
    """A plan describing what `governance apply` would do.

    The plan is intentionally a read-only dataclass; callers decide
    whether to execute it via `execute_apply_plan`.
    """

    repo: RepoRef
    branch: str
    action: ApplyAction
    declared_rules: BranchProtectionRules
    drift_report: DriftReport
    manual_followups: tuple[str, ...] = field(default_factory=tuple)

    @property
    def is_noop(self) -> bool:
        return self.action == ApplyAction.NOOP


@dataclass(frozen=True)
class ApplyResult:
    """The outcome of executing an ApplyPlan."""

    plan: ApplyPlan
    executed: bool
    error_message: str | None = None


def build_apply_plan(
    *,
    repo: RepoRef,
    branch: str,
    declared_rules: BranchProtectionRules,
    drift_report: DriftReport,
) -> ApplyPlan:
    """Produce an ApplyPlan from a DriftReport.

    The plan is NOOP when there's no drift at all; UPDATE when live
    protection exists but needs changes; CREATE when live protection
    is missing entirely.
    """
    if drift_report.live_unprotected:
        action = ApplyAction.CREATE
    elif drift_report.has_drift:
        action = ApplyAction.UPDATE
    else:
        action = ApplyAction.NOOP

    manual_followups = _collect_manual_followups()

    return ApplyPlan(
        repo=repo,
        branch=branch,
        action=action,
        declared_rules=declared_rules,
        drift_report=drift_report,
        manual_followups=manual_followups,
    )


def execute_apply_plan(
    plan: ApplyPlan,
    *,
    dry_run: bool = False,
    gh_command: str = "gh",
) -> ApplyResult:
    """Apply the plan to GitHub. A dry-run returns ApplyResult(executed=False).

    Never issues a PUT for a NOOP plan, even in non-dry-run mode.
    This is the core of the idempotency guarantee: if nothing would
    change, nothing is sent over the wire.
    """
    if plan.is_noop:
        return ApplyResult(plan=plan, executed=False, error_message=None)

    if dry_run:
        return ApplyResult(plan=plan, executed=False, error_message=None)

    try:
        put_branch_protection(
            plan.repo,
            plan.branch,
            plan.declared_rules,
            gh_command=gh_command,
        )
    except GovernanceApiError as exc:
        return ApplyResult(plan=plan, executed=False, error_message=str(exc))

    return ApplyResult(plan=plan, executed=True, error_message=None)


def _collect_manual_followups() -> tuple[str, ...]:
    """Manual followups Shipyard cannot apply via API.

    For v0.1.4 this is just the immutable-releases checkbox. Future
    releases add tag protection exceptions, workflow permissions
    edge cases, etc.

    The list is always returned (even when every item is a no-op)
    because the planning doc's "apply prints the followup checklist
    every time" rule keeps users in the loop about the gap between
    API-manageable and UI-only settings.
    """
    return (
        "Immutable releases: verify the 'Immutable releases' checkbox "
        "is enabled at https://github.com/<owner>/<repo>/settings. "
        "Shipyard cannot read this setting via API on personal repos.",
    )
