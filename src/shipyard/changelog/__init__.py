"""Shipyard post-release docs sync — changelog generator + post-tag hook runner.

Two independently usable capabilities:

- :mod:`shipyard.changelog.generator` — opinionated, shipyard-owned
  CHANGELOG.md + release-notes renderer that walks ``v*`` tags in reverse
  semver order and formats Keep-a-Changelog-flavoured output.
- :mod:`shipyard.changelog.hook` — unopinionated post-tag command
  runner: executes the configured command, watches files, commits with
  trailers + ``[skip ci]``, and handles the rebase-retry race.

Opt-in is via ``[release.changelog]`` and ``[release.post_tag_hook]`` in
``.shipyard/config.toml``. Absent sections = no behavior change.
"""

from __future__ import annotations

from shipyard.changelog.generator import (
    ChangelogConfig,
    Entry,
    build_entries,
    discover_tags,
    load_changelog_config,
    merges_between,
    render_changelog,
    render_release_notes,
)
from shipyard.changelog.hook import (
    HookConfig,
    HookResult,
    load_hook_config,
    run_hook,
)

__all__ = [
    "ChangelogConfig",
    "Entry",
    "HookConfig",
    "HookResult",
    "build_entries",
    "discover_tags",
    "load_changelog_config",
    "load_hook_config",
    "merges_between",
    "render_changelog",
    "render_release_notes",
    "run_hook",
]
