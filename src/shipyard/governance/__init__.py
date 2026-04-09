"""Governance — declarative branch protection, tag protection, and profile management.

See `profiles.py` for the solo/multi/custom profile definitions and
`status.py` for the drift-detection rollup. The public surface of
this package is intentionally narrow: CLI commands and tests should
import from `shipyard.governance.<module>` directly.
"""

from shipyard.governance.apply import (
    ApplyAction,
    ApplyPlan,
    ApplyResult,
    build_apply_plan,
    execute_apply_plan,
)
from shipyard.governance.compare import (
    DriftEntry,
    DriftReport,
    DriftStatus,
    compute_drift,
)
from shipyard.governance.config import (
    GovernanceConfig,
    load_governance_config,
    resolve_branch_rules,
)
from shipyard.governance.github import (
    GovernanceApiError,
    RepoRef,
    detect_repo_from_remote,
    get_branch_protection,
    put_branch_protection,
)
from shipyard.governance.profiles import (
    BranchProtectionRules,
    Profile,
    ProfileName,
    multi_profile,
    profile_for_name,
    solo_profile,
)
from shipyard.governance.status import (
    GovernanceStatus,
    build_status,
    format_status_text,
)

__all__ = [
    "ApplyAction",
    "ApplyPlan",
    "ApplyResult",
    "BranchProtectionRules",
    "DriftEntry",
    "DriftReport",
    "DriftStatus",
    "GovernanceApiError",
    "GovernanceConfig",
    "GovernanceStatus",
    "Profile",
    "ProfileName",
    "RepoRef",
    "build_apply_plan",
    "build_status",
    "compute_drift",
    "detect_repo_from_remote",
    "execute_apply_plan",
    "format_status_text",
    "get_branch_protection",
    "load_governance_config",
    "multi_profile",
    "profile_for_name",
    "put_branch_protection",
    "resolve_branch_rules",
    "solo_profile",
]
