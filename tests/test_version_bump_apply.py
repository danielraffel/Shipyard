"""Tests for `scripts/version_bump_check.py::apply_bumps`.

Focus: the #70 patch auto-apply flag. We import the script
directly by path (it's not part of the shipyard package) and
assert the honor-or-skip behavior.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pytest

SCRIPTS_DIR = Path(__file__).resolve().parent.parent / "scripts"


def _load_module():
    # Python 3.14 + dataclasses doesn't like the import-from-path
    # approach used in importlib.util.spec_from_file_location, so
    # we put the scripts dir on sys.path and plain-import once,
    # caching the resulting module.
    if str(SCRIPTS_DIR) not in sys.path:
        sys.path.insert(0, str(SCRIPTS_DIR))
    import version_bump_check  # type: ignore[import-not-found]

    return version_bump_check


def _init_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("GIT_AUTHOR_NAME", "T")
    monkeypatch.setenv("GIT_AUTHOR_EMAIL", "t@t")
    monkeypatch.setenv("GIT_COMMITTER_NAME", "T")
    monkeypatch.setenv("GIT_COMMITTER_EMAIL", "t@t")
    subprocess.check_call(
        ["git", "init", "--quiet", "--initial-branch=main"],
        cwd=tmp_path,
    )
    # Seed a single version file and a first commit.
    (tmp_path / "pyproject.toml").write_text(
        'name = "x"\nversion = "0.3.0"\n'
    )
    subprocess.check_call(["git", "add", "."], cwd=tmp_path)
    subprocess.check_call(
        ["git", "commit", "-q", "-m", "seed"], cwd=tmp_path
    )
    # Tag that commit so `git describe --tags` can find a baseline.
    subprocess.check_call(
        ["git", "tag", "v0.3.0"], cwd=tmp_path
    )
    return tmp_path


def _build_verdict(mod, *, final_level: str, auto_apply_patch: bool):
    surface = mod.Surface(
        name="cli",
        label="Test CLI",
        version_files=[
            mod.VersionFile(path="pyproject.toml", kind="pyproject_version"),
        ],
        trigger_paths=["**"],
        public_api_paths=[],
        internal_only_paths=[],
        changelog=None,
        auto_apply_patch=auto_apply_patch,
    )
    return mod.Verdict(
        surface=surface,
        heuristic=final_level,
        trailer_override=None,
        current_version="0.3.0",
        final_level=final_level,
    )


class TestAutoApplyPatch:
    def test_patch_skipped_when_flag_off(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # #70 regression fence: default behavior preserved.
        mod = _load_module()
        repo = _init_repo(tmp_path, monkeypatch)
        verdict = _build_verdict(
            mod, final_level="patch", auto_apply_patch=False
        )
        edited = mod.apply_bumps([verdict], "HEAD", repo)
        assert edited == []
        assert "0.3.0" in (repo / "pyproject.toml").read_text()

    def test_patch_applied_when_flag_on(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # #70 main: auto_apply_patch=true bumps the surface even
        # when the verdict is patch-only.
        mod = _load_module()
        repo = _init_repo(tmp_path, monkeypatch)
        verdict = _build_verdict(
            mod, final_level="patch", auto_apply_patch=True
        )
        edited = mod.apply_bumps([verdict], "HEAD", repo)
        assert "pyproject.toml" in edited
        assert "0.3.1" in (repo / "pyproject.toml").read_text()
        assert "0.3.0" not in (repo / "pyproject.toml").read_text()

    def test_minor_still_applied_regardless(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Minor-apply behavior is untouched by the flag.
        mod = _load_module()
        repo = _init_repo(tmp_path, monkeypatch)
        verdict = _build_verdict(
            mod, final_level="minor", auto_apply_patch=False
        )
        edited = mod.apply_bumps([verdict], "HEAD", repo)
        assert "pyproject.toml" in edited
        assert "0.4.0" in (repo / "pyproject.toml").read_text()

    def test_none_never_applied(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Even with auto_apply_patch=true, a `none` verdict is
        # a firm "nothing moved" — don't bump.
        mod = _load_module()
        repo = _init_repo(tmp_path, monkeypatch)
        verdict = _build_verdict(
            mod, final_level="none", auto_apply_patch=True
        )
        edited = mod.apply_bumps([verdict], "HEAD", repo)
        assert edited == []
        assert "0.3.0" in (repo / "pyproject.toml").read_text()
