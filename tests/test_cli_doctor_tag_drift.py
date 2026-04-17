"""Tests for `shipyard doctor` tag-drift reporting.

Exercises `_check_tag_drift` — the surface half of issue #70 that
catches maintainers eye-balling the release chain.
"""

from __future__ import annotations

from typing import TYPE_CHECKING

from shipyard.cli import _check_tag_drift

if TYPE_CHECKING:
    import pytest


def _fake_subprocess(
    describe_stdout: str = "v0.8.0",
    describe_rc: int = 0,
    rev_list_stdout: str = "0",
    rev_list_rc: int = 0,
):
    """Return a subprocess.run stand-in tailored to the drift check.

    The check makes two gh calls; this closure dispatches based on
    which subcommand is invoked.
    """
    def fake(cmd, *a, **kw):
        class R:
            returncode = 0
            stdout = ""
            stderr = ""

        joined = " ".join(cmd) if isinstance(cmd, list) else str(cmd)
        if "describe" in joined:
            R.returncode = describe_rc
            R.stdout = describe_stdout + "\n"
        elif "rev-list" in joined:
            R.returncode = rev_list_rc
            R.stdout = rev_list_stdout + "\n"
        return R

    return fake


class TestTagDrift:
    def test_no_tag_returns_none(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(
            "shipyard.cli.subprocess.run",
            _fake_subprocess(describe_rc=128, describe_stdout=""),
        )
        assert _check_tag_drift() is None

    def test_zero_commits_reports_up_to_date(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(
            "shipyard.cli.subprocess.run",
            _fake_subprocess(rev_list_stdout="0"),
        )
        section = _check_tag_drift()
        assert section is not None
        entry = section["tag_drift"]
        assert entry["ok"] is True
        assert "up-to-date" in entry["version"]

    def test_below_threshold_is_advisory(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(
            "shipyard.cli.subprocess.run",
            _fake_subprocess(rev_list_stdout="1"),
        )
        section = _check_tag_drift(warn_threshold=3)
        assert section is not None
        entry = section["tag_drift"]
        assert entry["ok"] is True
        assert "1 commit(s) ahead" in entry["version"]

    def test_at_threshold_flags_drift(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(
            "shipyard.cli.subprocess.run",
            _fake_subprocess(rev_list_stdout="5"),
        )
        section = _check_tag_drift(warn_threshold=3)
        assert section is not None
        entry = section["tag_drift"]
        assert entry["ok"] is False
        assert "5 commits ahead" in entry["version"]
        assert "issue #70" in entry["detail"]

    def test_bad_count_returns_none(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.setattr(
            "shipyard.cli.subprocess.run",
            _fake_subprocess(rev_list_stdout="not a number"),
        )
        assert _check_tag_drift() is None
