"""Snapshot governance state to TOML and restore from it.

The snapshot file is a check-in-able TOML document that captures
the **live** GitHub state at a point in time. Three purposes:

1. **Audit trail** — diff against the snapshot to see what changed
   since the last export.
2. **Drift catch** — `governance status` can compare live state
   against the snapshot, not just against the profile.
3. **Bootstrap from snapshot** — `governance apply --from snap.toml`
   reapplies a past state, useful for disaster recovery.

Security note: the snapshot does NOT contain secret values.
GitHub's API doesn't return them, and Shipyard intentionally never
sees them. Future versions may list secret *names* so recovery
prompts the user for each one by name; v0.1.5 focuses on branch
protection only, so there's nothing secret to capture yet.
"""

from __future__ import annotations

import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

if sys.version_info >= (3, 11):
    import tomllib
else:
    import tomli as tomllib

import tomli_w

from shipyard.governance.profiles import BranchProtectionRules

if TYPE_CHECKING:
    from shipyard.governance.github import RepoRef


SNAPSHOT_SCHEMA_VERSION = 1


@dataclass(frozen=True)
class GovernanceSnapshot:
    """A captured point-in-time view of a repo's governance state."""

    repo_slug: str
    exported_at: str
    schema_version: int = SNAPSHOT_SCHEMA_VERSION
    branches: dict[str, BranchProtectionRules] = field(default_factory=dict)

    def to_toml(self) -> str:
        """Serialize the snapshot to a TOML document (deterministic key order)."""
        doc: dict[str, Any] = {
            "shipyard_governance_snapshot": {
                "schema_version": self.schema_version,
                "repo": self.repo_slug,
                "exported_at": self.exported_at,
            },
        }
        # Branches sorted alphabetically so consecutive exports are
        # byte-identical when live state hasn't changed.
        branches_block: dict[str, Any] = {}
        for branch_name in sorted(self.branches):
            rules = self.branches[branch_name]
            branches_block[branch_name] = _rules_to_dict(rules)
        if branches_block:
            doc["branch_protection"] = branches_block
        return tomli_w.dumps(doc)

    @classmethod
    def from_toml(cls, text: str) -> GovernanceSnapshot:
        """Parse a snapshot back from TOML. Raises ValueError on schema mismatch."""
        data = tomllib.loads(text)
        header = data.get("shipyard_governance_snapshot")
        if not isinstance(header, dict):
            raise ValueError(
                "Not a Shipyard governance snapshot: missing "
                "[shipyard_governance_snapshot] header"
            )
        version = int(header.get("schema_version", 0))
        if version != SNAPSHOT_SCHEMA_VERSION:
            raise ValueError(
                f"Snapshot schema version {version} is not supported "
                f"(expected {SNAPSHOT_SCHEMA_VERSION})"
            )
        repo_slug = str(header.get("repo", ""))
        if not repo_slug:
            raise ValueError("Snapshot missing required field: repo")
        exported_at = str(header.get("exported_at", ""))

        branches: dict[str, BranchProtectionRules] = {}
        bp = data.get("branch_protection", {})
        if isinstance(bp, dict):
            for branch_name, block in bp.items():
                if not isinstance(block, dict):
                    continue
                branches[str(branch_name)] = _rules_from_dict(block)

        return cls(
            repo_slug=repo_slug,
            exported_at=exported_at,
            schema_version=version,
            branches=branches,
        )


def build_snapshot(
    *,
    repo: RepoRef,
    live_branches: dict[str, BranchProtectionRules],
    clock: Any = None,
) -> GovernanceSnapshot:
    """Assemble a GovernanceSnapshot from already-fetched live state.

    The `clock` parameter is injectable so tests can pin
    `exported_at` to a deterministic value. Default: UTC now.
    """
    now = clock() if clock else datetime.now(timezone.utc)
    return GovernanceSnapshot(
        repo_slug=repo.slug,
        exported_at=now.strftime("%Y-%m-%dT%H:%M:%SZ"),
        branches=dict(live_branches),
    )


def _rules_to_dict(rules: BranchProtectionRules) -> dict[str, Any]:
    """Translate a BranchProtectionRules instance to a plain TOML dict."""
    return {
        "require_pr": rules.require_pr,
        "require_status_checks": list(rules.require_status_checks),
        "require_strict_status": rules.require_strict_status,
        "require_review_count": rules.require_review_count,
        "enforce_admins": rules.enforce_admins,
        "dismiss_stale_reviews": rules.dismiss_stale_reviews,
        "require_code_owner_reviews": rules.require_code_owner_reviews,
        "allow_force_push": rules.allow_force_push,
        "allow_deletions": rules.allow_deletions,
        "require_linear_history": rules.require_linear_history,
        "required_conversation_resolution": rules.required_conversation_resolution,
    }


def _rules_from_dict(data: dict[str, Any]) -> BranchProtectionRules:
    """Translate a parsed TOML dict back into a BranchProtectionRules."""
    checks = data.get("require_status_checks", ())
    if isinstance(checks, list):
        checks = tuple(str(c) for c in checks)
    return BranchProtectionRules(
        require_pr=bool(data.get("require_pr", True)),
        require_status_checks=checks,
        require_strict_status=bool(data.get("require_strict_status", False)),
        require_review_count=int(data.get("require_review_count", 0)),
        enforce_admins=bool(data.get("enforce_admins", False)),
        dismiss_stale_reviews=bool(data.get("dismiss_stale_reviews", False)),
        require_code_owner_reviews=bool(data.get("require_code_owner_reviews", False)),
        allow_force_push=bool(data.get("allow_force_push", False)),
        allow_deletions=bool(data.get("allow_deletions", False)),
        require_linear_history=bool(data.get("require_linear_history", False)),
        required_conversation_resolution=bool(
            data.get("required_conversation_resolution", False),
        ),
    )
