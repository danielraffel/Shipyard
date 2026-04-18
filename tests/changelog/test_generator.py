"""Generator tests — walks real temp git repos, no mocking."""

from __future__ import annotations

from pathlib import Path

import pytest

from shipyard.changelog.generator import (
    ChangelogConfig,
    DEFAULT_SKIP_PATTERNS,
    build_entries,
    discover_tags,
    merges_between,
    render_changelog,
    render_release_notes,
)

from tests.changelog.conftest import commit, seed_repo, tag


def _cfg(**overrides: object) -> ChangelogConfig:
    base = {
        "enabled": True,
        "repo_url": "https://github.com/danielraffel/sample",
        "product": "Sample",
        "tag_filter": "v*",
        "skip_commit_patterns": DEFAULT_SKIP_PATTERNS,
    }
    base.update(overrides)
    return ChangelogConfig(**base)  # type: ignore[arg-type]


def test_discover_tags_filters_prereleases(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "initial (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")
    commit(repo, "b.txt", "b", "feat: b (#2)", date="2026-01-02T00:00:00+00:00")
    tag(repo, "v0.2.0", date="2026-01-02T00:00:00+00:00")
    tag(repo, "v1.0.0-rc1", date="2026-01-02T00:00:00+00:00")

    tags = discover_tags(_cfg(), cwd=repo)
    assert tags == ["v0.2.0", "v0.1.0"]  # prerelease dropped; newest first


def test_merges_between_skips_bump_patterns(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: keep (#1)", date="2026-01-01T00:00:00+00:00")
    commit(
        repo,
        "b.txt",
        "b",
        "chore: bump versions (#2)",
        date="2026-01-02T00:00:00+00:00",
    )
    commit(repo, "c.txt", "c", "fix: also keep (#3)", date="2026-01-03T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-03T00:00:00+00:00")

    cfg = _cfg()
    prs = merges_between(None, "v0.1.0", cfg.skip_patterns, cwd=repo)
    numbers = [n for n, _ in prs]
    assert numbers == [3, 1]  # newest-first order from `git log`; bump dropped


def test_build_entries_omits_empty_versions(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: initial (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")
    # Only a bump commit between tags → no user-visible merges.
    commit(
        repo,
        "v.txt",
        "v",
        "chore: bump versions (#2)",
        date="2026-01-02T00:00:00+00:00",
    )
    tag(repo, "v0.2.0", date="2026-01-02T00:00:00+00:00")
    commit(repo, "c.txt", "c", "fix: real (#3)", date="2026-01-03T00:00:00+00:00")
    tag(repo, "v0.3.0", date="2026-01-03T00:00:00+00:00")

    entries = build_entries(_cfg(), cwd=repo)
    versions = [e.version for e in entries]
    assert versions == ["0.3.0", "0.1.0"]  # 0.2.0 dropped


def test_render_changelog_is_idempotent(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: a (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")
    commit(repo, "b.txt", "b", "feat: b (#2)", date="2026-01-02T00:00:00+00:00")
    tag(repo, "v0.2.0", date="2026-01-02T00:00:00+00:00")

    cfg = _cfg()
    first = render_changelog(build_entries(cfg, cwd=repo), cfg)
    second = render_changelog(build_entries(cfg, cwd=repo), cfg)
    assert first == second


def test_render_changelog_reverse_chronological_order(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: a (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")
    commit(repo, "b.txt", "b", "feat: b (#2)", date="2026-01-02T00:00:00+00:00")
    tag(repo, "v0.2.0", date="2026-01-02T00:00:00+00:00")

    cfg = _cfg()
    text = render_changelog(build_entries(cfg, cwd=repo), cfg)
    idx_v2 = text.index("[0.2.0]")
    idx_v1 = text.index("[0.1.0]")
    assert idx_v2 < idx_v1


def test_anchor_stability_matches_pulp_format(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: a (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.13.1", date="2026-01-01T00:00:00+00:00")

    cfg = _cfg(product="Pulp", repo_url="https://github.com/danielraffel/pulp")
    entries = build_entries(cfg, cwd=repo)
    text = render_changelog(entries, cfg)
    # Anchor format must be byte-identical to pulp's `_anchor`:
    # "v" + version with dots stripped.
    assert '<a id="v0131"></a>' in text


def test_render_release_notes_contains_prev_link(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: a (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")
    commit(repo, "b.txt", "b", "feat: b (#2)", date="2026-01-02T00:00:00+00:00")
    tag(repo, "v0.2.0", date="2026-01-02T00:00:00+00:00")

    cfg = _cfg()
    entries = build_entries(cfg, cwd=repo)
    # entries[0] is newest (v0.2.0); its `prev` is v0.1.0.
    notes = render_release_notes(entries[0], entries[1], cfg)
    assert "What's new in v0.2.0" in notes
    assert "Previous release" in notes
    assert "v0.1.0" in notes


def test_render_release_notes_no_prev_for_first_release(tmp_path: Path) -> None:
    repo = seed_repo(tmp_path)
    commit(repo, "a.txt", "a", "feat: a (#1)", date="2026-01-01T00:00:00+00:00")
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")

    cfg = _cfg()
    entries = build_entries(cfg, cwd=repo)
    notes = render_release_notes(entries[0], None, cfg)
    assert "Previous release" not in notes


def test_pulp_byte_identical_golden(tmp_path: Path) -> None:
    """Tight golden for the pulp migration gate.

    Seeds a repo shaped like pulp would see for a two-version slice
    and checks the rendered header + entry exactly matches pulp's
    generator's shape.
    """
    repo = seed_repo(tmp_path)
    commit(
        repo,
        "a.txt",
        "a",
        "feat(cli): initial (#1)",
        date="2026-01-01T00:00:00+00:00",
    )
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")
    commit(
        repo,
        "b.txt",
        "b",
        "fix(cli): bug (#2)",
        date="2026-01-02T00:00:00+00:00",
    )
    tag(repo, "v0.2.0", date="2026-01-02T00:00:00+00:00")

    cfg = _cfg(product="Pulp", repo_url="https://github.com/danielraffel/pulp")
    text = render_changelog(build_entries(cfg, cwd=repo), cfg)

    expected = (
        "# Changelog\n"
        "\n"
        "All notable changes to Pulp are documented here. Each entry links\n"
        "to its [GitHub Release](https://github.com/danielraffel/pulp/releases).\n"
        "\n"
        "<!-- This file is auto-regenerated by the shipyard post-release docs sync hook\n"
        "     (shipyard changelog regenerate). Edits are picked up on the next regen as\n"
        "     long as they land in the right release's bullet block. See\n"
        "     docs/post-release-sync.md for the full end-to-end flow. -->\n"
        "\n"
        '<a id="v020"></a>\n'
        "## [0.2.0] - 2026-01-02\n"
        "\n"
        "- fix(cli): bug ([#2](https://github.com/danielraffel/pulp/pull/2))\n"
        "\n"
        '<a id="v010"></a>\n'
        "## [0.1.0] - 2026-01-01\n"
        "\n"
        "- feat(cli): initial ([#1](https://github.com/danielraffel/pulp/pull/1))\n"
        "\n"
        "[0.2.0]: https://github.com/danielraffel/pulp/releases/tag/v0.2.0\n"
        "[0.1.0]: https://github.com/danielraffel/pulp/releases/tag/v0.1.0\n"
    )
    assert text == expected


def test_legacy_merge_commit_subject(tmp_path: Path) -> None:
    """History predating squash-merge policy — ``Merge pull request`` form."""
    repo = seed_repo(tmp_path)
    commit(
        repo,
        "a.txt",
        "a",
        "Merge pull request #42 from someuser/fix-branch-name",
        date="2026-01-01T00:00:00+00:00",
    )
    tag(repo, "v0.1.0", date="2026-01-01T00:00:00+00:00")

    cfg = _cfg()
    entries = build_entries(cfg, cwd=repo)
    assert len(entries) == 1
    assert entries[0].prs == [(42, "fix branch name")]
