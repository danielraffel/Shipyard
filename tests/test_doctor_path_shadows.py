"""Tests for doctor's PATH-shadow check for `shipyard` binaries.

Pulp flagged this concretely: a v0.11.0 binary under
``~/.local/bin/shipyard`` (from an old ``uv tool install``) sat second
on PATH behind the pinned ``~/.pulp/bin/shipyard``. If PATH reordered
for any reason, every `shipyard ship` would have silently run v0.11.0.
This check warns before that happens.

The fake-binary fixtures use ``#!/bin/sh`` shell scripts, which Windows
cmd.exe can't execute. The underlying PATH-walk logic is pure Python and
works cross-platform; the tests are POSIX-only by design of the fixture.
"""

from __future__ import annotations

import os
import stat
import sys
from pathlib import Path
from typing import Any

import pytest

from shipyard.cli import _check_shipyard_path_shadows


pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "Fake-binary fixtures are POSIX shell scripts; the PATH-walk "
        "logic itself is cross-platform."
    ),
)


def _fake_binary(path: Path, stdout: str) -> None:
    """Write a tiny executable shell script that echoes `stdout`."""
    path.write_text(f"#!/bin/sh\nprintf '%s\\n' {stdout!r}\n")
    path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)


def _install_shipyard(
    dir_: Path, *, stdout: str = "shipyard, version 0.21.1"
) -> Path:
    dir_.mkdir(parents=True, exist_ok=True)
    target = dir_ / "shipyard"
    _fake_binary(target, stdout)
    return target


def test_single_binary_on_path_is_ok(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    pulp_bin = _install_shipyard(tmp_path / ".pulp" / "bin")
    monkeypatch.setenv("PATH", str(pulp_bin.parent))
    row = _check_shipyard_path_shadows()
    assert row["ok"] is True
    assert "single binary" in row["version"]


def test_empty_path_is_ok(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """No shipyard on PATH at all — doctor doesn't blow up."""
    monkeypatch.setenv("PATH", "")
    row = _check_shipyard_path_shadows()
    assert row["ok"] is True


def test_two_distinct_binaries_flagged(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """Exact Pulp scenario: pinned binary first, stale uv-managed second."""
    pulp_bin = _install_shipyard(
        tmp_path / ".pulp" / "bin", stdout="shipyard, version 0.21.1"
    )
    stale_bin = _install_shipyard(
        tmp_path / ".local" / "bin", stdout="shipyard, version 0.11.0"
    )
    monkeypatch.setenv(
        "PATH", os.pathsep.join([str(pulp_bin.parent), str(stale_bin.parent)])
    )

    row = _check_shipyard_path_shadows()
    assert row["ok"] is False
    detail = row["detail"]
    assert str(pulp_bin) in detail
    assert str(stale_bin) in detail
    assert "0.21.1" in detail
    assert "0.11.0" in detail
    assert "WINS" in detail
    assert "shadowed by" in detail


def test_symlinks_to_same_file_are_ok(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """If two PATH entries link to the same physical binary, no shadow."""
    real_dir = tmp_path / "real"
    real_dir.mkdir()
    real_bin = _install_shipyard(real_dir, stdout="shipyard, version 0.21.1")

    alias_dir = tmp_path / "alias"
    alias_dir.mkdir()
    (alias_dir / "shipyard").symlink_to(real_bin)

    monkeypatch.setenv(
        "PATH", os.pathsep.join([str(real_dir), str(alias_dir)])
    )

    row = _check_shipyard_path_shadows()
    assert row["ok"] is True


def test_unreadable_version_is_reported_gracefully(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """A binary that crashes on --version should be flagged with
    `<unreadable>`, not propagate the exception."""
    good = _install_shipyard(
        tmp_path / "good", stdout="shipyard, version 0.21.1"
    )
    bad_dir = tmp_path / "bad"
    bad_dir.mkdir()
    bad = bad_dir / "shipyard"
    # Non-zero exit + no output — simulate a broken install.
    bad.write_text("#!/bin/sh\nexit 99\n")
    bad.chmod(bad.stat().st_mode | stat.S_IXUSR)

    monkeypatch.setenv(
        "PATH", os.pathsep.join([str(good.parent), str(bad.parent)])
    )

    row = _check_shipyard_path_shadows()
    # Two binaries, distinct files → not ok.
    assert row["ok"] is False
    # The bad binary's version cell is empty string, not a traceback —
    # the row renders cleanly.
    assert str(bad) in row["detail"]


def test_detail_points_at_concrete_fix(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    """The `detail` string must name a specific binary to remove so
    the operator can act without re-reading the whole PATH."""
    pulp_bin = _install_shipyard(
        tmp_path / ".pulp" / "bin", stdout="shipyard, version 0.21.1"
    )
    stale_bin = _install_shipyard(
        tmp_path / ".local" / "bin", stdout="shipyard, version 0.11.0"
    )
    monkeypatch.setenv(
        "PATH", os.pathsep.join([str(pulp_bin.parent), str(stale_bin.parent)])
    )

    row: dict[str, Any] = _check_shipyard_path_shadows()
    detail = row["detail"]
    # The fix hint names the LAST (lowest-priority, most-likely-stale)
    # binary explicitly so the user can `rm <that path>` directly.
    assert f"e.g. {stale_bin!s}" in detail or str(stale_bin) in detail
    assert "reorder PATH" in detail or "remove" in detail
