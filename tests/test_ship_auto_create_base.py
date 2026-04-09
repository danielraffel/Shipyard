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
