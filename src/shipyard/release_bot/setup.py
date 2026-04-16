"""State detection, plan rendering, and GitHub interaction helpers.

This module is intentionally side-effect-thin — every function that
touches the network, the filesystem, or subprocesses is at the
bottom of the file and tagged in its docstring. The pure-logic
functions (plan_setup, render_pat_creation_url, describe_state) are
tested directly with fixture data.

Flow overview:

    detect_state(repo)  --->  ReleaseBotState      (reads gh secret list)
    plan_setup(state)   --->  SetupPlan            (pure)
    render_pat_creation_url(owner, repo, name)     (pure)
    set_secret(repo, token)                        (invokes gh)
    verify_token(repo, workflow_id)                (invokes gh)
"""

from __future__ import annotations

import json
import os
import subprocess
import urllib.parse
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any

# ── Data types ─────────────────────────────────────────────────────────────


@dataclass(frozen=True)
class ReleaseBotState:
    """Snapshot of what the environment currently looks like.

    Fields are None when undetermined. The CLI renders every known
    field; tests assert the shape precisely so regressions surface
    as compile-time dataclass drift.
    """

    repo_slug: str
    secret_present: bool
    secret_updated_at: datetime | None = None
    last_auto_release_conclusion: str | None = None  # "success"|"failure"|None
    last_auto_release_error_signature: str | None = None  # e.g. "auth"
    other_repos_with_secret: list[str] = field(default_factory=list)


@dataclass(frozen=True)
class SetupPlan:
    """The three-way choice the wizard offers.

    `recommended` is the default the wizard preselects. The set is
    a function of `ReleaseBotState` so integration tests can assert
    the recommendation without running any interactive prompts.
    """

    recommended: str  # "create-new" | "expand-existing" | "paste-existing"
    suggested_pat_name: str  # e.g., "my-app-release-bot" or "shipyard-release-bot"
    reasoning: str  # one-line explanation shown to the user


# ── Pure logic ─────────────────────────────────────────────────────────────


def describe_state(state: ReleaseBotState) -> list[str]:
    """Render a ReleaseBotState as ordered human-readable lines.

    Keeps all formatting in one function so the CLI layer stays
    mechanical. Lines are returned in display order.
    """
    lines = [f"repo: {state.repo_slug}"]
    if state.secret_present:
        when = (
            state.secret_updated_at.strftime("%Y-%m-%d")
            if state.secret_updated_at
            else "unknown"
        )
        lines.append(f"RELEASE_BOT_TOKEN: configured (set {when})")
    else:
        lines.append("RELEASE_BOT_TOKEN: missing")

    if state.last_auto_release_conclusion:
        tag = state.last_auto_release_conclusion
        if state.last_auto_release_error_signature == "auth":
            lines.append(
                f"last auto-release: {tag} (rejected at actions/checkout — "
                "PAT scope or secret value drift)"
            )
        else:
            lines.append(f"last auto-release: {tag}")

    if state.other_repos_with_secret:
        others = ", ".join(state.other_repos_with_secret)
        lines.append(f"other repos with RELEASE_BOT_TOKEN: {others}")
    return lines


def plan_setup(
    state: ReleaseBotState, *, shared_name: str | None = None
) -> SetupPlan:
    """Compute the default path and PAT name given current state.

    Rules:
    - If the user supplied --shared-name, honor it (advanced).
    - If there are other repos already using this secret, recommend
      expanding that PAT's Selected-repositories list to include
      this repo (reuse path).
    - Otherwise recommend creating a fresh per-project PAT.
    """
    if shared_name:
        return SetupPlan(
            recommended="create-new",
            suggested_pat_name=shared_name,
            reasoning=(
                f"Using shared PAT name '{shared_name}' as requested. "
                "Include every Shipyard consumer repo in its Selected "
                "repositories list."
            ),
        )

    repo_name = state.repo_slug.split("/", 1)[-1].lower()
    suggested = f"{repo_name}-release-bot"

    if state.other_repos_with_secret and not state.secret_present:
        return SetupPlan(
            recommended="expand-existing",
            suggested_pat_name=suggested,
            reasoning=(
                "You already have RELEASE_BOT_TOKEN on another repo "
                f"({state.other_repos_with_secret[0]}). Reusing that "
                "PAT by adding this repo to its Selected repositories "
                "list avoids a second rotation point. Create a fresh "
                "per-project PAT instead if you prefer least privilege."
            ),
        )

    return SetupPlan(
        recommended="create-new",
        suggested_pat_name=suggested,
        reasoning=(
            "A fresh per-project PAT is the least-privilege default — "
            "one compromised token affects one repo. Use --shared-name "
            "shipyard-release-bot if you'd rather rotate a single PAT "
            "across all Shipyard consumers."
        ),
    )


def render_pat_creation_url(
    *, owner: str, pat_name: str, repo: str, expiration_days: int = 365
) -> str:
    """Build a pre-filled URL for GitHub's fine-grained PAT creation form.

    GitHub accepts a subset of query parameters on /settings/personal-
    access-tokens/new — we populate name, description, and the
    repository-scope hint. The user still clicks through the
    permissions UI; we surface the required values in the CLI so
    they have something to double-check against.
    """
    params = {
        "type": "beta",
        "name": pat_name,
        "description": f"Shipyard release bot for {owner}/{repo}",
        "expires_in": str(expiration_days),
        "target_name": owner,
    }
    return (
        "https://github.com/settings/personal-access-tokens/new?"
        + urllib.parse.urlencode(params)
    )


# ── Side-effecting helpers ─────────────────────────────────────────────────
#
# Each function below touches `gh`. They return structured results
# (or raise typed exceptions) so the CLI layer doesn't parse stdout.


class ReleaseBotError(Exception):
    """Raised when an operation can't complete for a user-fixable reason.

    Carries a short message suitable for CLI display (first line) and
    an optional detail (follow-up lines). Test fixtures assert on
    `error.message` directly so phrasing stays stable.
    """

    def __init__(self, message: str, detail: str = "") -> None:
        super().__init__(message)
        self.message = message
        self.detail = detail


def detect_state(
    repo_slug: str, *, known_repos_hint: list[str] | None = None
) -> ReleaseBotState:
    """Read the current RELEASE_BOT_TOKEN situation from `gh`.

    Best-effort: returns a minimally-populated ReleaseBotState if
    any `gh` call fails rather than raising, so the wizard still
    runs in environments where not every API is reachable.
    """
    secret_present = False
    secret_updated_at: datetime | None = None
    secrets = _list_secrets(repo_slug)
    if secrets is not None:
        for s in secrets:
            if s.get("name") == "RELEASE_BOT_TOKEN":
                secret_present = True
                secret_updated_at = _parse_ts(s.get("updated_at"))
                break

    last_conclusion: str | None = None
    last_error_sig: str | None = None
    last_run = _last_auto_release(repo_slug)
    if last_run is not None:
        last_conclusion = last_run.get("conclusion") or None
        if last_conclusion == "failure":
            last_error_sig = _detect_checkout_auth_failure(
                repo_slug, int(last_run["databaseId"])
            )

    others: list[str] = []
    if known_repos_hint:
        for other in known_repos_hint:
            if other == repo_slug:
                continue
            found = _list_secrets(other)
            if found is None:
                continue
            if any(s.get("name") == "RELEASE_BOT_TOKEN" for s in found):
                others.append(other)

    return ReleaseBotState(
        repo_slug=repo_slug,
        secret_present=secret_present,
        secret_updated_at=secret_updated_at,
        last_auto_release_conclusion=last_conclusion,
        last_auto_release_error_signature=last_error_sig,
        other_repos_with_secret=others,
    )


def set_secret(repo_slug: str, token: str) -> None:
    """Push `token` as RELEASE_BOT_TOKEN via `gh secret set --body -`.

    Token is piped on stdin — never appears in argv, never written
    to a file, never logged. Raises ReleaseBotError if `gh` reports
    failure.
    """
    if not token or not token.strip():
        raise ReleaseBotError(
            "Refusing to set an empty RELEASE_BOT_TOKEN.",
            "Paste the full token value when prompted.",
        )
    try:
        result = subprocess.run(
            ["gh", "secret", "set", "RELEASE_BOT_TOKEN", "--repo", repo_slug,
             "--body", "-"],
            input=token,
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.SubprocessError, FileNotFoundError) as exc:
        raise ReleaseBotError(
            "Couldn't run `gh secret set`.",
            f"{exc}. Install gh and authenticate with `gh auth login`.",
        ) from exc
    if result.returncode != 0:
        raise ReleaseBotError(
            "gh secret set failed.",
            result.stderr.strip() or "No stderr. Check `gh auth status`.",
        )


def verify_token(repo_slug: str, *, workflow_file: str = "auto-release.yml") -> str:
    """Dispatch a real run of the release workflow to confirm checkout works.

    Returns the conclusion string ("success"/"failure"/...). Raises
    ReleaseBotError if the dispatch itself fails (distinct from the
    workflow's own pass/fail).

    The conclusion we care about is the first job's outcome, not
    the whole workflow — the tag-push step intentionally no-ops on
    "no version bump," but actions/checkout having succeeded is
    proof the PAT works. We report the workflow-level conclusion
    and let the caller interpret it.

    Correctness note (#51 P1): we record the timestamp just before
    `gh workflow run` returns and only accept a run whose createdAt
    is strictly after that mark. Otherwise a pre-existing completed
    run would satisfy the poll immediately and produce a stale
    verdict.

    Precision note (#55 P1): GitHub's `createdAt` field is reported
    at second precision, so we floor the dispatch mark to the same
    resolution before comparing. Without this, a run created in the
    same wall-clock second as `dispatch_mark` would have a
    parsed-microsecond value of zero and would appear "older" than
    the mark, causing the poll to time out on a run that in fact
    succeeded.
    """
    # Floor to second precision to match `gh`'s createdAt resolution.
    raw_mark = datetime.now(timezone.utc)
    dispatch_mark = raw_mark.replace(microsecond=0)
    try:
        dispatch = subprocess.run(
            ["gh", "workflow", "run", workflow_file, "--repo", repo_slug,
             "--ref", _default_branch(repo_slug) or "main"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.SubprocessError, FileNotFoundError) as exc:
        raise ReleaseBotError(
            "Couldn't dispatch verification workflow.",
            str(exc),
        ) from exc
    if dispatch.returncode != 0:
        raise ReleaseBotError(
            "gh workflow run failed.",
            dispatch.stderr.strip()
            or "The workflow may not accept workflow_dispatch.",
        )

    # Poll for a completed run whose createdAt is strictly newer
    # than the dispatch mark. Stale completed runs are ignored.
    for _ in range(30):  # ~5 min @ 10s
        latest = _last_workflow_run(repo_slug, workflow_file)
        if latest and latest.get("status") == "completed":
            created = _parse_ts(latest.get("createdAt"))
            if created is not None and created >= dispatch_mark:
                return latest.get("conclusion") or "unknown"
        import time

        time.sleep(10)
    raise ReleaseBotError(
        "Verification workflow didn't complete in 5 min.",
        "Check Actions tab manually.",
    )


# ── Internals ──────────────────────────────────────────────────────────────


def _list_secrets(repo_slug: str) -> list[dict[str, Any]] | None:
    try:
        result = subprocess.run(
            ["gh", "api", f"repos/{repo_slug}/actions/secrets",
             "--paginate"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if result.returncode != 0 or not result.stdout.strip():
        return None
    try:
        data = json.loads(result.stdout)
    except json.JSONDecodeError:
        return None
    return list(data.get("secrets", []))


def _last_auto_release(repo_slug: str) -> dict[str, Any] | None:
    return _last_workflow_run(repo_slug, "auto-release.yml")


def _last_workflow_run(
    repo_slug: str, workflow_file: str
) -> dict[str, Any] | None:
    try:
        result = subprocess.run(
            ["gh", "run", "list", "--workflow", workflow_file,
             "--repo", repo_slug, "--limit", "1",
             "--json", "databaseId,status,conclusion,createdAt"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if result.returncode != 0:
        return None
    try:
        arr = json.loads(result.stdout)
    except json.JSONDecodeError:
        return None
    return arr[0] if arr else None


def _detect_checkout_auth_failure(
    repo_slug: str, run_id: int
) -> str | None:
    """Classify a failed run by peeking at the log for the checkout step.

    Returns "auth" if the failure is the well-known "could not read
    Username" signature, else None. Used for the doctor drifted/
    rejected diagnosis.
    """
    try:
        result = subprocess.run(
            ["gh", "run", "view", str(run_id), "--repo", repo_slug,
             "--log-failed"],
            capture_output=True,
            text=True,
            timeout=30,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if result.returncode != 0:
        return None
    if "could not read Username" in result.stdout:
        return "auth"
    return None


def _default_branch(repo_slug: str) -> str | None:
    try:
        result = subprocess.run(
            ["gh", "repo", "view", repo_slug, "--json", "defaultBranchRef",
             "--jq", ".defaultBranchRef.name"],
            capture_output=True,
            text=True,
            timeout=15,
        )
    except (subprocess.SubprocessError, FileNotFoundError):
        return None
    if result.returncode != 0:
        return None
    return result.stdout.strip() or None


def _parse_ts(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00"))
    except (ValueError, TypeError):
        return None


# ── Open browser ────────────────────────────────────────────────────────────


def open_browser(url: str) -> bool:
    """Try to open `url` in the user's browser. Returns True on success.

    Silently false when headless — the caller should print the URL
    too so the user can copy-paste it.
    """
    if os.environ.get("SHIPYARD_NO_BROWSER"):
        return False
    for cmd in (["open", url], ["xdg-open", url], ["start", url]):
        try:
            rc = subprocess.run(
                cmd, capture_output=True, timeout=5
            ).returncode
        except (subprocess.SubprocessError, FileNotFoundError):
            continue
        if rc == 0:
            return True
    return False
