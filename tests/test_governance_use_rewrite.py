"""Tests for the `_rewrite_profile_in_config` helper used by `governance use`."""

from __future__ import annotations

from typing import TYPE_CHECKING

from shipyard.cli import _rewrite_profile_in_config

if TYPE_CHECKING:
    from pathlib import Path


def _write(tmp_path: Path, content: str) -> Path:
    p = tmp_path / "config.toml"
    p.write_text(content)
    return p


# ── Rewrite existing profile line ──────────────────────────────────────


def test_rewrite_replaces_existing_profile_line(tmp_path: Path) -> None:
    config = _write(tmp_path, '[project]\nname = "test"\nprofile = "solo"\n')
    _rewrite_profile_in_config(config, "multi")
    text = config.read_text()
    assert 'profile' in text and 'multi' in text
    assert 'solo' not in text
    assert 'name = "test"' in text  # other fields preserved


def test_rewrite_preserves_leading_whitespace(tmp_path: Path) -> None:
    config = _write(
        tmp_path,
        '[project]\n    profile = "solo"\n',
    )
    _rewrite_profile_in_config(config, "multi")
    text = config.read_text()
    # Leading spaces preserved on the rewritten line
    assert any(
        line.startswith("    profile") and "multi" in line
        for line in text.splitlines()
    )


# ── Insert profile when missing ────────────────────────────────────────


def test_rewrite_inserts_profile_when_missing(tmp_path: Path) -> None:
    config = _write(tmp_path, '[project]\nname = "test"\n')
    _rewrite_profile_in_config(config, "multi")
    text = config.read_text()
    assert 'profile' in text and 'multi' in text
    # Profile line should come immediately after [project]
    lines = text.splitlines()
    project_idx = lines.index("[project]")
    assert "profile" in lines[project_idx + 1]
    assert "multi" in lines[project_idx + 1]


# ── Only edits the project section ─────────────────────────────────────


def test_rewrite_does_not_touch_profile_in_other_sections(tmp_path: Path) -> None:
    """A `profile` key outside [project] must be left alone."""
    content = (
        '[project]\n'
        'profile = "solo"\n'
        '\n'
        '[runtime]\n'
        'profile = "base"\n'  # different key, same name, different section
    )
    config = _write(tmp_path, content)
    _rewrite_profile_in_config(config, "multi")
    text = config.read_text()
    assert text.count('"multi"') == 1   # only the project one changed
    assert text.count('"base"') == 1    # runtime.profile untouched


# ── Idempotency ────────────────────────────────────────────────────────


def test_rewrite_idempotent(tmp_path: Path) -> None:
    config = _write(tmp_path, '[project]\nprofile = "solo"\n')
    _rewrite_profile_in_config(config, "multi")
    first = config.read_text()
    _rewrite_profile_in_config(config, "multi")
    second = config.read_text()
    assert first == second
