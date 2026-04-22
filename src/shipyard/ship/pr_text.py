"""Compose PR titles + bodies for auto-opened PRs.

Goal: an auto-opened PR should be indistinguishable in quality from
a human-written one. That means the title should describe **what**
changed (not 'Ship feature/foo'), and the body should carry the
context the author already wrote in the commit message.

Principles (see memory ``feedback_no_branding`` + ``feedback_no_ship_in_user_text``):

* Never prefix the title with "Ship" — that's an internal vocabulary
  leak into artifacts reviewers read.
* Never append "Automated by Shipyard." — the reviewer doesn't need
  to know what tool opened the PR.
* Pull title + body from the most recent *meaningful* commit, not
  the tip. `shipyard pr` always tags on a ``chore: bump versions``
  commit at the tip so version bumps get stamped into git; that
  commit's subject and body are mechanical filler and must never
  surface as the PR's title/body. Walk back past them to the
  feature commit the author actually wrote.
* Fall back to a prettified branch name when no meaningful commit
  is recoverable (shallow clone, detached HEAD, git failure).

Kept in a dedicated module so both the ``shipyard pr`` path in
``cli.py`` and the ``shipyard ship`` path in ``ship/merge.py`` share
the same implementation — drift between the two was the root cause
of pulp#621 (empty body) and pulp#616 (branding trailer still there).
"""

from __future__ import annotations

import subprocess
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from shipyard.core.config import Config
    from shipyard.ship.lane_policy import LanePolicy


# Subjects that are known to be mechanical/bot commits `shipyard pr`
# (or similar tooling) tags on. When the tip commit starts with any
# of these prefixes, we walk back to find the real author commit.
_MECHANICAL_SUBJECT_PREFIXES = (
    "chore: bump versions",
    "chore(plugin): bump",
    "chore(release):",
    "chore: regenerate changelog",
    "docs: regenerate changelog",
)

# Depth cap on the walk. Branches with more than a handful of
# consecutive mechanical commits aren't a real scenario; if every
# commit in the range is mechanical, fall back to the branch name.
_MEANINGFUL_COMMIT_WALK_DEPTH = 20


def compose_pr_title(branch: str) -> str:
    """Return a PR title for ``branch``.

    Prefers the most recent *meaningful* commit's subject line.
    Falls back to a prettified branch name when git can't answer
    or every recent commit is mechanical.
    """
    ref = _meaningful_commit_ref()
    if ref is not None:
        subject = _commit_field(ref, "%s")
        if subject:
            return subject
    return _branch_fallback(branch)


def compose_pr_body(
    *,
    config: Config | None = None,
    policy: LanePolicy | None = None,
) -> str:
    """Return a PR body for a freshly-opened PR.

    Body shape:
      1. Most recent *meaningful* commit's body text (everything
         after the subject line), if present. Authors routinely
         explain the 'why' in the commit body — surfacing it in the
         PR makes the PR self-contained without a second click.
         Mechanical tip commits (``chore: bump versions``) are
         skipped so their empty body doesn't stomp the feature body.
      2. Advisory-lanes section, if the resolved lane policy marks
         any lanes advisory. Reviewers need to know a red advisory
         lane didn't block merge. Lane overrides via Lane-Policy
         trailer are called out so reviewers can audit why the
         default policy was flipped.

    Either ``config`` OR a pre-resolved ``policy`` may be passed.
    Passing neither yields a body with only the commit body.
    """
    lines: list[str] = []
    ref = _meaningful_commit_ref()
    if ref is not None:
        commit_body = _commit_field(ref, "%b")
        if commit_body:
            lines.append(commit_body)

    resolved_policy = policy or _resolve_policy_or_none(config)
    if resolved_policy is not None:
        advisory = sorted(resolved_policy.advisory_targets)
        if advisory:
            if lines:
                lines.append("")
            lines.append("## Advisory lanes")
            lines.append(
                "The following lanes are **advisory** — their status is "
                "informational and does not block merge:"
            )
            for target in advisory:
                suffix = (
                    " (overridden via Lane-Policy trailer)"
                    if target in resolved_policy.overrides_from_trailer
                    else ""
                )
                lines.append(f"- `{target}`{suffix}")

    return "\n".join(lines)


def _meaningful_commit_ref() -> str | None:
    """Walk back from HEAD, returning the first commit ref whose
    subject is not a known mechanical bump/release filler.

    Returns ``None`` when every commit in the walk window is
    mechanical, when git fails, or when the repo has no commits.
    """
    for depth in range(_MEANINGFUL_COMMIT_WALK_DEPTH):
        ref = "HEAD" if depth == 0 else f"HEAD~{depth}"
        subject = _commit_field(ref, "%s")
        if not subject:
            # Either git failed or we walked past the branch root.
            return None
        if not _is_mechanical_subject(subject):
            return ref
    return None


def _is_mechanical_subject(subject: str) -> bool:
    lowered = subject.strip().lower()
    return any(
        lowered.startswith(prefix.lower())
        for prefix in _MECHANICAL_SUBJECT_PREFIXES
    )


def _commit_field(ref: str, fmt: str) -> str:
    try:
        result = subprocess.run(
            ["git", "log", "-1", f"--format={fmt}", ref],
            capture_output=True, text=True, check=True, timeout=5,
        )
        return result.stdout.strip()
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired, OSError):
        return ""


def _branch_fallback(branch: str) -> str:
    """feature/foo-bar → 'Foo bar'. Last-resort title for when git
    won't cooperate."""
    name = branch.split("/")[-1]
    name = name.replace("-", " ").replace("_", " ")
    return name.capitalize() or branch


def _resolve_policy_or_none(config: Config | None) -> LanePolicy | None:
    """Resolve the lane policy without forcing a hard dep on it when
    callers don't care about advisory lanes."""
    if config is None:
        return None
    from shipyard.ship.lane_policy import resolve_lane_policy

    return resolve_lane_policy(
        config,
        known_targets=list((config.targets or {}).keys()),
    )
