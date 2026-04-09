"""Tests for `shipyard ship --base` auto-create-base behavior."""

from __future__ import annotations

from shipyard.cli import _should_auto_create_base

# ── _should_auto_create_base defaults ──────────────────────────────────


def test_default_on_for_develop_pattern() -> None:
    assert _should_auto_create_base("develop/foo", None) is True
    assert _should_auto_create_base("develop/auth/rewrite", None) is True


def test_default_on_for_release_pattern() -> None:
    assert _should_auto_create_base("release/v1.0", None) is True


def test_default_off_for_main() -> None:
    assert _should_auto_create_base("main", None) is False


def test_default_off_for_feature() -> None:
    assert _should_auto_create_base("feature/whatever", None) is False


def test_default_off_for_master() -> None:
    assert _should_auto_create_base("master", None) is False


# ── Explicit flag overrides default ────────────────────────────────────


def test_explicit_true_overrides_default_off() -> None:
    assert _should_auto_create_base("feature/x", True) is True


def test_explicit_false_overrides_default_on() -> None:
    assert _should_auto_create_base("develop/x", False) is False


def test_explicit_false_for_main_stays_false() -> None:
    assert _should_auto_create_base("main", False) is False


# ── JSON mode suppresses human output in auto-create helper ────────────


def test_auto_create_base_respects_json_mode(capsys) -> None:
    """When ctx.json_mode is True, the helper must not emit human text.

    Codex flagged that `_maybe_auto_create_base_branch` called
    `render_message` unconditionally, which would interleave
    human progress lines with the final JSON envelope and break
    `shipyard ship --json` for machine consumers.
    """
    import subprocess
    from unittest.mock import patch

    from shipyard.cli import _maybe_auto_create_base_branch

    class FakeCtx:
        json_mode = True
        config = None  # not touched in the paths under test

    # Short-circuit the helper via ls-remote returning "not found"
    # (code 2), then have detect_repo_from_remote return None. That
    # path would normally call render_message; the json_mode guard
    # must suppress it.
    def fake_run(*args, **kwargs):
        return subprocess.CompletedProcess(
            args=[], returncode=2, stdout="", stderr="",
        )

    with patch("subprocess.run", side_effect=fake_run), patch(
        "shipyard.governance.detect_repo_from_remote",
        return_value=None,
    ):
        _maybe_auto_create_base_branch(FakeCtx(), "develop/foo")

    captured = capsys.readouterr()
    assert captured.out == ""
    assert captured.err == ""
