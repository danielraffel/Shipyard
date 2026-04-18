"""Changelog generator — ports pulp's `regenerate_changelog.py` to a
shipyard-parameterized module.

Walks every tag matching ``tag_filter`` in reverse-chronological order,
extracts merged PRs in each tag's range, and emits a
Keep-a-Changelog-flavoured document with release-page backlinks.

Versions that contain zero user-facing merges (only `chore: bump`
commits) are omitted — we don't publish an empty entry.

Idempotent: running twice produces byte-identical output.

The anchor format, entry format, and reference-link footer are kept
byte-identical to pulp's hardcoded generator so pulp's migration
produces identical output once the config values are plugged in.
"""

from __future__ import annotations

import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, NamedTuple

if TYPE_CHECKING:
    from collections.abc import Iterable

    from shipyard.core.config import Config

# Pulp's three default skip patterns. The shipyard config can override,
# but these are the safe defaults and match what pulp shipped with.
DEFAULT_SKIP_PATTERNS: tuple[str, ...] = (
    r"^chore: bump .*version",
    r"^chore\(release\): ",
    r"^bump .*to v?\d+\.\d+\.\d+$",
)


@dataclass
class ChangelogConfig:
    """Parameters for the generator. Loaded from ``[release.changelog]``."""

    enabled: bool = False
    repo_url: str = ""
    path: str = "CHANGELOG.md"
    tag_filter: str = "v*"
    product: str = "this project"
    skip_commit_patterns: tuple[str, ...] = DEFAULT_SKIP_PATTERNS
    title: str = "Changelog"

    # Compiled patterns — built once, reused.
    _compiled_skip: tuple[re.Pattern[str], ...] = field(
        default_factory=tuple, init=False, repr=False
    )

    def __post_init__(self) -> None:
        self._compiled_skip = tuple(
            re.compile(p, re.IGNORECASE) for p in self.skip_commit_patterns
        )

    @property
    def skip_patterns(self) -> tuple[re.Pattern[str], ...]:
        return self._compiled_skip


def load_changelog_config(config: Config) -> ChangelogConfig:
    """Extract ``[release.changelog]`` from a loaded shipyard Config.

    Returns a ``ChangelogConfig`` with ``enabled=False`` if the section
    is absent. Callers should treat ``enabled=False`` as opt-out.
    """
    section = config.get("release.changelog")
    if not isinstance(section, dict):
        return ChangelogConfig(enabled=False)

    skip = section.get("skip_commit_patterns")
    if not isinstance(skip, list) or not skip:
        skip_tuple: tuple[str, ...] = DEFAULT_SKIP_PATTERNS
    else:
        skip_tuple = tuple(str(p) for p in skip)

    return ChangelogConfig(
        enabled=bool(section.get("enabled", False)),
        repo_url=str(section.get("repo_url", "")),
        path=str(section.get("path", "CHANGELOG.md")),
        tag_filter=str(section.get("tag_filter", "v*")),
        product=str(section.get("product", "this project")),
        skip_commit_patterns=skip_tuple,
        title=str(section.get("title", "Changelog")),
    )


class Entry(NamedTuple):
    """One rendered changelog entry — a single tag with its PR bullets."""

    version: str  # "0.13.1"
    tag: str  # "v0.13.1"
    date: str  # "2026-04-16"
    prs: list[tuple[int, str]]  # [(259, "fix(cli): ..."), ...]


# ---- git helpers ---------------------------------------------------


def _run_git(args: list[str], cwd: Path | None = None) -> str:
    """Run a git command and return stripped stdout.

    Raises ``subprocess.CalledProcessError`` on non-zero exit.
    """
    return subprocess.check_output(
        ["git", *args],
        text=True,
        cwd=str(cwd) if cwd else None,
    ).strip()


def discover_tags(cfg: ChangelogConfig, *, cwd: Path | None = None) -> list[str]:
    """Return tags matching ``cfg.tag_filter`` in reverse semver order.

    We additionally restrict to strictly vMAJOR.MINOR.PATCH format so
    pre-release suffixes (``v1.0.0-rc1``) and plugin-prefixed tags
    (``plugin-v*``) don't leak into the CHANGELOG. Projects that want
    a different shape can narrow ``tag_filter`` — but even then, we
    keep the strict post-filter so a lone ``-rc`` doesn't render as a
    release entry.
    """
    out = _run_git(
        ["tag", "--list", cfg.tag_filter, "--sort=-v:refname"],
        cwd=cwd,
    )
    # Derive a strict version-match regex from the filter's prefix.
    # `v*` → `^v\d+\.\d+\.\d+$`; `cli-v*` → `^cli-v\d+\.\d+\.\d+$`.
    prefix = cfg.tag_filter.rstrip("*")
    strict = re.compile(rf"^{re.escape(prefix)}\d+\.\d+\.\d+$")
    return [t for t in out.splitlines() if strict.fullmatch(t)]


def tag_date(tag: str, *, cwd: Path | None = None) -> str:
    """Return ISO date (``YYYY-MM-DD``) the tag's commit was authored."""
    iso = _run_git(["log", "-1", "--format=%cI", tag], cwd=cwd)
    return iso[:10]


def _version_from_tag(tag: str, tag_filter: str) -> str:
    """Strip the tag-filter prefix to get the bare version string."""
    prefix = tag_filter.rstrip("*")
    if prefix and tag.startswith(prefix):
        return tag[len(prefix):]
    # Fallback: chop a leading "v" if present.
    return tag[1:] if tag.startswith("v") else tag


def merges_between(
    previous: str | None,
    current: str,
    skip_patterns: Iterable[re.Pattern[str]],
    *,
    cwd: Path | None = None,
) -> list[tuple[int, str]]:
    """Return ``(PR number, subject)`` pairs for merges in (previous, current].

    Handles both:

    - Squash-merge subjects (``"docs: foo (#123)"``) — GitHub's default.
    - Legacy merge commits (``"Merge pull request #N from owner/branch"``).

    Subjects matching any compiled ``skip_patterns`` are dropped.
    """
    range_ = f"{previous}..{current}" if previous else current
    try:
        out = _run_git(
            ["log", range_, "--first-parent", "--pretty=format:%s"],
            cwd=cwd,
        )
    except subprocess.CalledProcessError:
        # Tag or range doesn't exist — callers can decide to skip.
        return []

    prs: list[tuple[int, str]] = []
    seen: set[int] = set()
    for line in out.splitlines():
        m = re.search(r"\s*\(#(\d+)\)\s*$", line)
        if m:
            number = int(m.group(1))
            subject = line[: m.start()].rstrip()
        else:
            m = re.match(r"^Merge pull request #(\d+) from .+?/(.+)$", line)
            if not m:
                continue
            number = int(m.group(1))
            subject = m.group(2).replace("-", " ").strip() or "Merge"

        if number in seen:
            continue
        if any(p.search(subject) for p in skip_patterns):
            continue
        seen.add(number)
        prs.append((number, subject))
    return prs


def build_entries(
    cfg: ChangelogConfig,
    *,
    cwd: Path | None = None,
) -> list[Entry]:
    """Walk every tag under ``cfg.tag_filter`` and build renderable entries.

    Empty versions (no user-visible merges) are omitted.
    """
    tags = discover_tags(cfg, cwd=cwd)
    entries: list[Entry] = []
    for i, tag in enumerate(tags):
        prev = tags[i + 1] if i + 1 < len(tags) else None
        prs = merges_between(prev, tag, cfg.skip_patterns, cwd=cwd)
        if not prs:
            continue
        entries.append(
            Entry(
                version=_version_from_tag(tag, cfg.tag_filter),
                tag=tag,
                date=tag_date(tag, cwd=cwd),
                prs=prs,
            )
        )
    return entries


# ---- renderers -----------------------------------------------------


def _anchor(entry: Entry) -> str:
    """Stable HTML anchor id for a version heading.

    Byte-identical to pulp's ``_anchor``: ``v`` + version with dots
    stripped. GitHub's auto-slug is unpredictable with brackets + em-
    dashes, so we emit an explicit ``<a id="...">`` tag.
    """
    return f"v{entry.version.replace('.', '')}"


def render_changelog(entries: list[Entry], cfg: ChangelogConfig) -> str:
    """Render the full CHANGELOG.md body."""
    lines: list[str] = [
        f"# {cfg.title}",
        "",
        f"All notable changes to {cfg.product} are documented here. Each entry links",
        f"to its [GitHub Release]({cfg.repo_url}/releases).",
        "",
        "<!-- This file is auto-regenerated by the shipyard post-release docs sync hook",
        "     (shipyard changelog regenerate). Edits are picked up on the next regen as",
        "     long as they land in the right release's bullet block. See",
        "     docs/post-release-sync.md for the full end-to-end flow. -->",
        "",
    ]
    for e in entries:
        lines.append(f'<a id="{_anchor(e)}"></a>')
        lines.append(f"## [{e.version}] - {e.date}")
        lines.append("")
        for number, subject in e.prs:
            lines.append(
                f"- {subject} ([#{number}]({cfg.repo_url}/pull/{number}))"
            )
        lines.append("")
    for e in entries:
        lines.append(f"[{e.version}]: {cfg.repo_url}/releases/tag/{e.tag}")
    lines.append("")
    return "\n".join(lines)


def render_release_notes(
    entry: Entry,
    prev: Entry | None,
    cfg: ChangelogConfig,
) -> str:
    """Render per-release markdown — fed into ``softprops/action-gh-release``."""
    lines: list[str] = [
        f"## What's new in {entry.tag}",
        "",
    ]
    for number, subject in entry.prs:
        lines.append(f"- {subject} (#{number})")
    lines.append("")
    lines.append(
        f"**Full changelog:** [CHANGELOG.md § {entry.version}]"
        f"({cfg.repo_url}/blob/main/CHANGELOG.md#{_anchor(entry)})"
    )
    if prev:
        lines.append(
            f"**Previous release:** [{prev.tag}]({cfg.repo_url}/releases/tag/{prev.tag})"
        )
    lines.append("")
    return "\n".join(lines)


# ---- top-level helpers --------------------------------------------


def regenerate(
    cfg: ChangelogConfig,
    *,
    cwd: Path | None = None,
) -> tuple[str, list[Entry]]:
    """Build entries and render the full CHANGELOG text.

    Returns ``(rendered_text, entries)`` so callers that want to
    subsequently render a specific release-notes blob don't need to
    walk the tag graph twice.
    """
    entries = build_entries(cfg, cwd=cwd)
    return render_changelog(entries, cfg), entries


def changelog_path(cfg: ChangelogConfig, *, cwd: Path | None = None) -> Path:
    """Resolve the absolute path of the changelog file for the repo."""
    base = cwd or Path.cwd()
    p = Path(cfg.path)
    return p if p.is_absolute() else base / p
