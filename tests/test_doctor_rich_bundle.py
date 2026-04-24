"""Tests for doctor's rich-bundle health smoke (#181).

The PyInstaller onefile bundle embeds rich's ``_unicode_data``
archive entry as a deflated blob. When that blob gets corrupted on
disk (partial extract after an interrupted install, XProtect
rewrite, Apple Silicon notarization race), the first ``cell_len``
call on a wide char raises ``zlib.error: Error -3 ... incorrect
header check`` — inside the summary-table renderer — and
``shipyard ship`` silently abandons with the PR already opened.

This check force-loads that table in doctor so the failure surfaces
with a reinstall hint rather than during a real ship.
"""

from __future__ import annotations

from typing import Any
from unittest.mock import patch

from shipyard.cli import _check_rich_bundle_health


def test_rich_bundle_healthy_returns_ok() -> None:
    # Baseline: on any machine where rich is installed correctly,
    # the smoke passes and doctor's Core section stays ready.
    result: dict[str, Any] = _check_rich_bundle_health()
    assert result["ok"] is True
    assert "OK" in result["version"] or "ok" in result["version"].lower()


def test_rich_bundle_zlib_corruption_surfaces_with_reinstall_hint() -> None:
    # The #181 failure shape: rich.cells.cell_len raises zlib.error
    # because the bundled _unicode_data archive entry is truncated.
    import zlib

    def _boom(_s: str) -> int:
        raise zlib.error("Error -3 while decompressing data: incorrect header check")

    with patch("rich.cells.cell_len", side_effect=_boom):
        result = _check_rich_bundle_health()

    assert result["ok"] is False
    assert "error" in result["version"].lower() or "failed" in result["version"].lower()
    # The reinstall hint must be in detail — that's the whole point
    # of catching the failure in doctor rather than mid-ship.
    assert "install.sh" in result["detail"]
    # And the exception type must be named so a support thread can
    # grep for "zlib" without asking the user to paste the traceback.
    assert "error" in result["version"].lower()


def test_rich_bundle_import_error_also_surfaces() -> None:
    # A genuinely broken bundle might fail the import itself rather
    # than the cell_len call. Doctor must report ok=False either way.
    def _boom(_s: str) -> int:
        raise ImportError("No module named 'rich._unicode_data._lookup'")

    with patch("rich.cells.cell_len", side_effect=_boom):
        result = _check_rich_bundle_health()

    assert result["ok"] is False
    assert "install.sh" in result["detail"]


def test_rich_bundle_unexpected_result_is_not_silently_ok() -> None:
    # Defense in depth: if cell_len returns something nonsensical
    # (a mock or a corrupt fallback table), doctor should still flag
    # rather than returning ok=True for a broken bundle.
    with patch("rich.cells.cell_len", return_value=0):
        result = _check_rich_bundle_health()
    assert result["ok"] is False
