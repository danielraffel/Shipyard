"""High-level `governance status` rollup.

This is the read-side of the governance surface. It ties the
profile → config → GitHub chain together into a single call that
the CLI can invoke. The CLI is intentionally thin on top of this:
it formats the report for humans or JSON but does no computation.
"""

from __future__ import annotations

from dataclasses import dataclass, field

from shipyard.governance.compare import DriftReport, compute_drift
from shipyard.governance.config import (
    GovernanceConfig,
    resolve_branch_rules,
)
from shipyard.governance.github import (
    GovernanceApiError,
    RepoRef,
    get_branch_protection,
)


@dataclass(frozen=True)
class GovernanceStatus:
    """Aggregate status across every governed branch."""

    repo: RepoRef
    profile_name: str
    reports: tuple[DriftReport, ...] = field(default_factory=tuple)
    errors: tuple[str, ...] = field(default_factory=tuple)

    @property
    def has_drift(self) -> bool:
        return any(r.has_drift for r in self.reports)

    @property
    def has_errors(self) -> bool:
        return bool(self.errors)


def build_status(
    *,
    repo: RepoRef,
    governance: GovernanceConfig,
    branches: tuple[str, ...],
    gh_command: str = "gh",
) -> GovernanceStatus:
    """Run the full read path: resolve rules, fetch live state, compute drift.

    Branches that fail to fetch are reported as errors in the result
    rather than raising — a single branch with a permission problem
    should not mask the rest of the matrix.
    """
    reports: list[DriftReport] = []
    errors: list[str] = []

    for branch in branches:
        declared_rules = resolve_branch_rules(governance, branch)
        try:
            live_rules = get_branch_protection(repo, branch, gh_command=gh_command)
        except GovernanceApiError as exc:
            errors.append(f"{branch}: {exc}")
            continue

        report = compute_drift(
            branch=branch,
            profile_rules=governance.profile.branch_protection,
            declared_rules=declared_rules,
            live_rules=live_rules,
        )
        reports.append(report)

    return GovernanceStatus(
        repo=repo,
        profile_name=governance.profile.name,
        reports=tuple(reports),
        errors=tuple(errors),
    )


def format_status_text(status: GovernanceStatus) -> str:
    """Render a GovernanceStatus as plain text for the CLI human output.

    Keeps the format terse — the full matrix is too wide for most
    terminals, so this prints a per-branch summary with drifted
    fields highlighted. Callers that want the full matrix can walk
    `status.reports` directly.
    """
    lines: list[str] = []
    lines.append(f"Project: {status.repo.slug}")
    lines.append(f"Profile: {status.profile_name}")
    if status.errors:
        lines.append(f"Errors: {len(status.errors)}")
        for err in status.errors:
            lines.append(f"  ! {err}")
    lines.append("")

    for report in status.reports:
        if report.live_unprotected:
            lines.append(f"  ✗ {report.branch}: UNPROTECTED (run: shipyard governance apply)")
            continue
        drifted = report.drifted_entries
        deviated = report.deviated_entries
        if not drifted and not deviated:
            lines.append(f"  ✓ {report.branch}: aligned with profile")
            continue
        if drifted:
            lines.append(f"  ✗ {report.branch}: {len(drifted)} field(s) drifted from config")
            for entry in drifted:
                lines.append(
                    f"      {entry.field_name}: "
                    f"config={entry.declared_value!r}, live={entry.live_value!r}"
                )
            lines.append("      fix: shipyard governance apply")
        if deviated and not drifted:
            lines.append(
                f"  ℹ {report.branch}: {len(deviated)} field(s) deviated from profile"
            )
            for entry in deviated:
                lines.append(
                    f"      {entry.field_name}: "
                    f"profile={entry.profile_value!r}, config={entry.declared_value!r}"
                )
    return "\n".join(lines)
