"""Create a branch and apply its governance rules in a single atomic-feeling step.

This closes the "new branch exists unprotected until someone
remembers to apply protection" gap in the `develop/*` workflow.
Calling `shipyard branch apply --create develop/foo` creates the
branch from the configured root (usually `main`), pushes it to
`origin`, and applies the matching `[branch_protection."<glob>"]`
rules — all before the function returns.

The atomicity is best-effort: if the branch is created but the
rule apply fails, the branch is left in place (so the user can
re-try apply) rather than deleted. The alternative — delete on
failure — risks losing a freshly-created branch over a transient
API glitch. The user gets a clear error telling them exactly what
to re-run.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from enum import Enum
from typing import TYPE_CHECKING

from shipyard.governance.github import (
    GovernanceApiError,
    put_branch_protection,
)

if TYPE_CHECKING:
    from shipyard.governance.github import RepoRef
    from shipyard.governance.profiles import BranchProtectionRules


class BranchCreateStatus(str, Enum):
    CREATED = "created"
    ALREADY_EXISTS = "already_exists"
    RULES_APPLIED = "rules_applied"
    RULES_FAILED = "rules_failed"
    GIT_FAILED = "git_failed"


@dataclass(frozen=True)
class BranchCreateResult:
    """What `branch apply --create` actually did."""

    branch: str
    status: BranchCreateStatus
    message: str | None = None
    rules_applied: BranchProtectionRules | None = None

    @property
    def ok(self) -> bool:
        return self.status in (
            BranchCreateStatus.CREATED,
            BranchCreateStatus.RULES_APPLIED,
        )


def create_branch_on_remote(
    *,
    branch: str,
    base_branch: str = "main",
    git_command: str = "git",
) -> BranchCreateResult:
    """Create `branch` from `base_branch` on origin, idempotently.

    Always resolves the base SHA from the remote via `ls-remote`,
    not from the local `refs/remotes/origin/<base>` tracking ref.
    That tracking ref can be absent (shallow/single-branch clones)
    or stale (long-lived worktrees), both of which would make a
    local-ref-based push either fail or create the wrong commit.

    If the branch already exists on the remote, returns
    `ALREADY_EXISTS` — callers can then fall through to apply rules
    without re-creating. If git fails (network, auth, invalid ref),
    returns `GIT_FAILED` with the stderr attached.
    """
    # Check whether the remote already has this branch.
    check = subprocess.run(
        [git_command, "ls-remote", "--exit-code", "--heads", "origin", branch],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if check.returncode == 0:
        return BranchCreateResult(
            branch=branch,
            status=BranchCreateStatus.ALREADY_EXISTS,
            message=f"Branch '{branch}' already exists on origin",
        )

    # `ls-remote --exit-code` returns 2 when the ref doesn't exist,
    # which is the path we want. Any other non-zero return is a
    # real error worth surfacing.
    if check.returncode not in (0, 2):
        return BranchCreateResult(
            branch=branch,
            status=BranchCreateStatus.GIT_FAILED,
            message=(
                f"ls-remote failed for {branch}: "
                f"{(check.stderr or '').strip() or 'no detail'}"
            ),
        )

    # Resolve the remote base SHA directly. This does NOT rely on
    # `refs/remotes/origin/<base>` being present locally — shallow
    # or single-branch clones won't have that ref at all, and stale
    # long-lived worktrees can have it pointing at an older commit.
    base_lookup = subprocess.run(
        [git_command, "ls-remote", "--exit-code", "origin", f"refs/heads/{base_branch}"],
        capture_output=True,
        text=True,
        timeout=30,
    )
    if base_lookup.returncode != 0:
        return BranchCreateResult(
            branch=branch,
            status=BranchCreateStatus.GIT_FAILED,
            message=(
                f"ls-remote failed to resolve base branch '{base_branch}': "
                f"{(base_lookup.stderr or '').strip() or 'no detail'}"
            ),
        )

    # Output is `<sha>\trefs/heads/<base_branch>` on the first line.
    first_line = (base_lookup.stdout or "").strip().splitlines()
    if not first_line:
        return BranchCreateResult(
            branch=branch,
            status=BranchCreateStatus.GIT_FAILED,
            message=(
                f"ls-remote returned no SHA for origin/{base_branch} — "
                f"does the base branch exist on the remote?"
            ),
        )
    base_sha = first_line[0].split(None, 1)[0]

    # Push the resolved SHA as the new branch. Using the raw SHA as
    # the push source avoids any dependency on local refs — git
    # sends the commit if the remote doesn't have it, and creates
    # the branch ref pointing at it.
    push = subprocess.run(
        [
            git_command,
            "push",
            "origin",
            f"{base_sha}:refs/heads/{branch}",
        ],
        capture_output=True,
        text=True,
        timeout=60,
    )
    if push.returncode != 0:
        return BranchCreateResult(
            branch=branch,
            status=BranchCreateStatus.GIT_FAILED,
            message=(
                f"git push failed creating {branch} from {base_branch} "
                f"({base_sha[:8]}): "
                f"{(push.stderr or '').strip() or 'no detail'}"
            ),
        )

    return BranchCreateResult(
        branch=branch,
        status=BranchCreateStatus.CREATED,
        message=(
            f"Created '{branch}' from '{base_branch}' "
            f"({base_sha[:8]}) on origin"
        ),
    )


def create_branch_and_apply_rules(
    *,
    repo: RepoRef,
    branch: str,
    base_branch: str,
    rules: BranchProtectionRules,
    git_command: str = "git",
    gh_command: str = "gh",
) -> BranchCreateResult:
    """The full flow: create the branch, then apply its rules.

    If the branch already exists, skip creation and apply rules
    anyway (idempotent — useful for fixing a prior half-completed
    create). If rule application fails, the branch is NOT deleted.
    """
    create_result = create_branch_on_remote(
        branch=branch,
        base_branch=base_branch,
        git_command=git_command,
    )
    if create_result.status == BranchCreateStatus.GIT_FAILED:
        return create_result

    # Whether we just created it or it already existed, apply rules.
    try:
        put_branch_protection(repo, branch, rules, gh_command=gh_command)
    except GovernanceApiError as exc:
        return BranchCreateResult(
            branch=branch,
            status=BranchCreateStatus.RULES_FAILED,
            message=(
                f"Branch exists but rule apply failed: {exc}. "
                f"Re-run `shipyard governance apply --branch {branch}` "
                f"to retry."
            ),
            rules_applied=None,
        )

    return BranchCreateResult(
        branch=branch,
        status=BranchCreateStatus.RULES_APPLIED,
        message=(
            f"Created '{branch}' from '{base_branch}' and applied governance rules"
            if create_result.status == BranchCreateStatus.CREATED
            else f"Branch '{branch}' already existed; reapplied governance rules"
        ),
        rules_applied=rules,
    )
