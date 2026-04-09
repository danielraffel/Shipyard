"""Thin wrappers around `gh api` for GitHub branch protection.

Shipyard already shells out to the `gh` CLI elsewhere (cloud
workflow dispatch, PR creation, merge). The governance commands
reuse the same pattern here rather than pulling in a direct REST
client, so:

- Authentication is inherited from `gh auth login`, which the user
  has already done and which doctor already checks.
- Rate limiting and retry policies are whatever `gh` does by
  default, which matches the rest of Shipyard's GitHub interactions.
- A missing or unauthenticated `gh` is surfaced with the same error
  message as other Shipyard commands.

The functions here deal only with translating the GitHub REST shape
into our `BranchProtectionRules` dataclass and back. Drift detection
and idempotency live in `compare.py` and `apply.py`.
"""

from __future__ import annotations

import json
import subprocess
from dataclasses import dataclass

from shipyard.governance.profiles import BranchProtectionRules


class GovernanceApiError(Exception):
    """A `gh api` call failed in an unrecoverable way.

    Used to distinguish "branch has no protection" (not an error,
    returns an empty rules object) from "the gh CLI isn't
    authenticated" (fatal, must surface to the user).
    """


@dataclass(frozen=True)
class RepoRef:
    """An owner/repo pair plus an optional branch name."""

    owner: str
    name: str

    @property
    def slug(self) -> str:
        return f"{self.owner}/{self.name}"


def get_branch_protection(
    repo: RepoRef,
    branch: str,
    *,
    gh_command: str = "gh",
) -> BranchProtectionRules | None:
    """Read the live branch protection for `branch` on `repo`.

    Returns None when the branch has no protection at all (404 from
    GitHub). Raises GovernanceApiError on any other failure so callers
    can distinguish "unprotected" from "inaccessible".
    """
    try:
        result = subprocess.run(
            [
                gh_command,
                "api",
                f"repos/{repo.slug}/branches/{branch}/protection",
            ],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError) as exc:
        raise GovernanceApiError(f"gh api timeout/missing: {exc}") from exc

    if result.returncode != 0:
        stderr = (result.stderr or "").strip()
        # A 404 specifically means "branch not protected" — translate
        # that into a clean None rather than an exception.
        if "Branch not protected" in stderr or "404" in stderr:
            return None
        raise GovernanceApiError(
            f"gh api failed for {repo.slug}/{branch}: {stderr or 'no detail'}"
        )

    try:
        payload = json.loads(result.stdout)
    except json.JSONDecodeError as exc:
        raise GovernanceApiError(f"gh api returned non-JSON: {exc}") from exc

    return _rules_from_api_payload(payload)


def put_branch_protection(
    repo: RepoRef,
    branch: str,
    rules: BranchProtectionRules,
    *,
    gh_command: str = "gh",
) -> None:
    """Apply `rules` to `branch` on `repo` via `gh api PUT`.

    Idempotency and diffing happen one layer up in `apply.py`; this
    function just translates the rules dataclass into the GitHub REST
    body and issues the PUT.
    """
    body = _api_payload_from_rules(rules)
    try:
        result = subprocess.run(
            [
                gh_command,
                "api",
                "-X",
                "PUT",
                f"repos/{repo.slug}/branches/{branch}/protection",
                "--input",
                "-",
            ],
            input=json.dumps(body),
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError) as exc:
        raise GovernanceApiError(f"gh api timeout/missing: {exc}") from exc

    if result.returncode != 0:
        raise GovernanceApiError(
            f"gh api PUT failed for {repo.slug}/{branch}: "
            f"{(result.stderr or '').strip() or 'no detail'}"
        )


def detect_repo_from_remote(
    *,
    git_command: str = "git",
    gh_command: str = "gh",
) -> RepoRef | None:
    """Best-effort guess of the repo owner/name from the git remote.

    Tries `gh repo view --json nameWithOwner` first (the canonical
    path when the user has the gh CLI set up), then falls back to
    parsing `git remote get-url origin`.
    """
    try:
        result = subprocess.run(
            [gh_command, "repo", "view", "--json", "nameWithOwner"],
            capture_output=True,
            text=True,
            timeout=15,
        )
        if result.returncode == 0:
            payload = json.loads(result.stdout)
            slug = payload.get("nameWithOwner", "")
            if "/" in slug:
                owner, name = slug.split("/", 1)
                return RepoRef(owner=owner, name=name)
    except (subprocess.SubprocessError, FileNotFoundError, json.JSONDecodeError):
        pass

    try:
        result = subprocess.run(
            [git_command, "remote", "get-url", "origin"],
            capture_output=True,
            text=True,
            timeout=5,
        )
        if result.returncode == 0:
            return _parse_git_remote_url(result.stdout.strip())
    except (subprocess.SubprocessError, FileNotFoundError):
        pass

    return None


def _parse_git_remote_url(url: str) -> RepoRef | None:
    """Parse github.com/owner/repo from an https or ssh remote URL."""
    if not url:
        return None
    cleaned = url.removesuffix(".git")
    # https://github.com/owner/repo
    if "github.com/" in cleaned:
        parts = cleaned.split("github.com/", 1)[1].split("/")
        if len(parts) >= 2:
            return RepoRef(owner=parts[0], name=parts[1])
    # git@github.com:owner/repo
    if "github.com:" in cleaned:
        parts = cleaned.split("github.com:", 1)[1].split("/")
        if len(parts) >= 2:
            return RepoRef(owner=parts[0], name=parts[1])
    return None


# ── Translation layer between GitHub REST and BranchProtectionRules ────


def _rules_from_api_payload(payload: dict) -> BranchProtectionRules:
    """Translate GitHub's branch-protection JSON into a rules object."""
    status_checks_block = payload.get("required_status_checks") or {}
    required_checks = tuple(status_checks_block.get("contexts", []) or ())
    strict = bool(status_checks_block.get("strict", False))

    prs_block = payload.get("required_pull_request_reviews") or {}
    review_count = int(prs_block.get("required_approving_review_count", 0) or 0)
    dismiss_stale = bool(prs_block.get("dismiss_stale_reviews", False))
    code_owner = bool(prs_block.get("require_code_owner_reviews", False))

    enforce_admins_block = payload.get("enforce_admins") or {}
    enforce_admins = bool(enforce_admins_block.get("enabled", False))

    allow_force_push_block = payload.get("allow_force_pushes") or {}
    allow_force_push = bool(allow_force_push_block.get("enabled", False))

    allow_deletions_block = payload.get("allow_deletions") or {}
    allow_deletions = bool(allow_deletions_block.get("enabled", False))

    require_linear_block = payload.get("required_linear_history") or {}
    require_linear = bool(require_linear_block.get("enabled", False))

    require_conversation_block = payload.get("required_conversation_resolution") or {}
    require_conversation = bool(require_conversation_block.get("enabled", False))

    # `require_pr` is true iff the required_pull_request_reviews
    # block exists at all. GitHub's schema conflates "require PRs"
    # with "require reviews > 0" but Shipyard treats them as two
    # distinct knobs.
    require_pr = bool(payload.get("required_pull_request_reviews"))

    return BranchProtectionRules(
        require_pr=require_pr,
        require_status_checks=required_checks,
        require_strict_status=strict,
        require_review_count=review_count,
        enforce_admins=enforce_admins,
        dismiss_stale_reviews=dismiss_stale,
        require_code_owner_reviews=code_owner,
        allow_force_push=allow_force_push,
        allow_deletions=allow_deletions,
        require_linear_history=require_linear,
        required_conversation_resolution=require_conversation,
    )


def _api_payload_from_rules(rules: BranchProtectionRules) -> dict:
    """Translate a rules dataclass into the GitHub REST PUT body."""
    # Required status checks block. GitHub rejects an empty
    # `contexts` list when `strict=true`, so omit the whole block
    # when there are no checks.
    required_status_checks: dict | None = None
    if rules.require_status_checks:
        required_status_checks = {
            "strict": rules.require_strict_status,
            "contexts": list(rules.require_status_checks),
        }

    # PR reviews block. Only emitted when require_pr is set OR a
    # review count is configured, so "no PRs required" translates to
    # a null block.
    pr_reviews: dict | None = None
    if rules.require_pr or rules.require_review_count > 0:
        pr_reviews = {
            "required_approving_review_count": rules.require_review_count,
            "dismiss_stale_reviews": rules.dismiss_stale_reviews,
            "require_code_owner_reviews": rules.require_code_owner_reviews,
        }

    return {
        "required_status_checks": required_status_checks,
        "enforce_admins": rules.enforce_admins,
        "required_pull_request_reviews": pr_reviews,
        "restrictions": None,
        "required_linear_history": rules.require_linear_history,
        "allow_force_pushes": rules.allow_force_push,
        "allow_deletions": rules.allow_deletions,
        "required_conversation_resolution": rules.required_conversation_resolution,
    }
