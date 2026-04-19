"""Lane degrade-mode: advisory vs required lanes.

A "lane" is a single validation target (e.g. ``windows``, ``mac``,
``linux``). By default every lane is **required** — its failure blocks
the merge gate. This module implements the per-target ``advisory``
escape hatch for matrices where one lane is intentionally noisy (a
flaky Windows runner, an experimental macOS-ARM64 lane, etc.).

Two sources feed into the final "is this lane required?" decision:

1. **Target config**: ``[targets.<name>] advisory = true`` in
   ``.shipyard/config.toml``. This is the steady-state preference
   for this lane.

2. **Commit trailer**: ``Lane-Policy: <target>=required`` or
   ``Lane-Policy: <target>=advisory`` on the tip commit of the PR.
   This overlays (1) for **this PR only**. It is parsed from the
   commit, not the PR body, so an agent can escalate one lane on a
   release candidate without touching the committed config.

The resolver composes (1) and (2) and returns the effective advisory
set. The ship merge gate then treats required-lane failures as
blocking and advisory-lane failures as informational (but surfaced in
watch / PR comment / JSON).

Why keep this separate from quarantine?
=======================================

Quarantine (``.shipyard/quarantine.toml``) and advisory mode answer
subtly different questions:

- **Quarantine**: "target T is quarantined because its runner is
  flaky; I want TEST / UNKNOWN failures suppressed, but INFRA /
  TIMEOUT / CONTRACT still block the merge." It is failure-class
  aware.
- **Advisory**: "lane T is advisory; I don't want its status to
  block the merge regardless of failure class." It is a strict
  policy knob that treats the whole lane as non-blocking.

They compose: a quarantined target is already a subset of advisory
in the TEST/UNKNOWN case; marking a target advisory in addition
widens the suppression to every failure class on that lane.
"""

from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass
from typing import TYPE_CHECKING

from shipyard.targets import is_advisory as _target_is_advisory

if TYPE_CHECKING:
    from collections.abc import Iterable

    from shipyard.core.config import Config


_TRAILER_LINE_RE = re.compile(
    r"^\s*Lane-Policy\s*:\s*(.+?)\s*$",
    re.IGNORECASE | re.MULTILINE,
)
_PAIR_RE = re.compile(
    r"(?P<target>[A-Za-z0-9_.\-]+)\s*=\s*(?P<policy>required|advisory)",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class LanePolicy:
    """Resolved per-target advisory/required verdict.

    ``advisory_targets`` is the set of target *names* whose failures
    should be treated as non-blocking. ``overrides_from_trailer`` is
    the subset of those decisions that came from a commit trailer
    (useful for logging: "escalated windows=required via trailer").
    """

    advisory_targets: frozenset[str]
    overrides_from_trailer: frozenset[str]

    def is_advisory(self, target: str) -> bool:
        return target in self.advisory_targets

    def is_required(self, target: str) -> bool:
        return target not in self.advisory_targets


def parse_trailer(commit_message: str) -> dict[str, str]:
    """Parse ``Lane-Policy`` trailers out of a commit message.

    Returns a dict mapping target name -> "required" | "advisory".
    Accepts multiple pairs per trailer line, space or comma separated:

        Lane-Policy: windows=advisory
        Lane-Policy: macos=required linux=advisory
        Lane-Policy: macos=required, linux=advisory

    Trailer lines are matched case-insensitively on the key but the
    *value* is normalized to lower-case ("required" / "advisory") to
    keep downstream comparisons simple. Unknown values are ignored.
    Later trailers win when the same target appears twice — matches
    how git itself treats repeated trailers (last-wins).
    """
    out: dict[str, str] = {}
    for m in _TRAILER_LINE_RE.finditer(commit_message or ""):
        payload = m.group(1)
        for pair in _PAIR_RE.finditer(payload):
            target = pair.group("target").strip()
            policy = pair.group("policy").strip().lower()
            if policy in {"required", "advisory"}:
                out[target] = policy
    return out


def read_tip_commit_message(
    sha: str | None = None, *, cwd: str | None = None
) -> str:
    """Return the commit message for ``sha`` (default HEAD).

    Uses ``git log -1 --format=%B`` so we see the full message
    including trailers. Never raises — on any failure we return an
    empty string and the caller treats it as "no trailer present".
    Tests that want deterministic trailer behavior pass the message
    directly to :func:`resolve_lane_policy` instead.
    """
    try:
        ref = sha or "HEAD"
        return subprocess.check_output(
            ["git", "log", "-1", "--format=%B", ref],
            text=True,
            stderr=subprocess.DEVNULL,
            cwd=cwd,
        )
    except (subprocess.CalledProcessError, FileNotFoundError):
        return ""


def advisory_targets_from_config(config: Config) -> set[str]:
    """Collect the set of target names whose config marks them advisory."""
    out: set[str] = set()
    targets = config.targets if hasattr(config, "targets") else {}
    for name, raw in (targets or {}).items():
        if isinstance(raw, dict) and _target_is_advisory(raw):
            out.add(name)
    return out


def resolve_lane_policy(
    config: Config,
    *,
    commit_message: str | None = None,
    known_targets: Iterable[str] | None = None,
) -> LanePolicy:
    """Compose config-level advisory lanes with a trailer overlay.

    Call sites that already have the tip commit message can pass it
    directly; otherwise this function reads HEAD on demand. A
    ``known_targets`` iterable scopes the result — trailer entries
    that name a target no longer in the config are dropped silently
    (typo-tolerant; prevents a stale trailer from widening the
    advisory set unexpectedly).
    """
    base_advisory = advisory_targets_from_config(config)
    if commit_message is None:
        commit_message = read_tip_commit_message()

    trailer = parse_trailer(commit_message)
    known = set(known_targets) if known_targets is not None else None

    advisory: set[str] = set(base_advisory)
    overrides: set[str] = set()

    for target, policy in trailer.items():
        if known is not None and target not in known:
            # Trailer names a target that no longer exists in the
            # config — ignore rather than raising, since the common
            # cause is a typo or a removed target.
            continue
        if policy == "required" and target in advisory:
            advisory.discard(target)
            overrides.add(target)
        elif policy == "advisory" and target not in advisory:
            advisory.add(target)
            overrides.add(target)

    return LanePolicy(
        advisory_targets=frozenset(advisory),
        overrides_from_trailer=frozenset(overrides),
    )


def advisory_platforms_for_config(
    config: Config, *, commit_message: str | None = None
) -> set[str]:
    """Map an advisory ``LanePolicy`` to the platform strings the merge
    gate uses as keys.

    The merge gate keys on ``platform`` (e.g. ``windows-arm64``), not
    target name — so we translate via the target config. A target
    with no ``platform`` is skipped (it has no merge-gate key to
    flip).
    """
    policy = resolve_lane_policy(
        config,
        commit_message=commit_message,
        known_targets=list((config.targets or {}).keys()),
    )
    out: set[str] = set()
    for name, raw in (config.targets or {}).items():
        if name not in policy.advisory_targets:
            continue
        if isinstance(raw, dict):
            platform = raw.get("platform")
            if platform:
                out.add(str(platform))
    return out


__all__ = [
    "LanePolicy",
    "advisory_platforms_for_config",
    "advisory_targets_from_config",
    "parse_trailer",
    "read_tip_commit_message",
    "resolve_lane_policy",
]
