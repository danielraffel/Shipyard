"""Preflight error annotation when daemon version is skewed (#197).

Before this, a CLI upgrade with a still-running older daemon produced
phantom "SSH target is unreachable (timeout)" errors during
`shipyard run/ship/pr` preflight. The user had no signal that the
real cause was daemon staleness — they'd spend time chasing
`~/.ssh/config` or network issues. These tests pin the annotation:

1. Daemon running at matching version → no note added.
2. Daemon running at different version → skew note in the error.
3. Daemon not running → no note (daemon-less is supported).
4. Daemon pre-v0.26.0 (no `shipyard_version` field) → skew note
   because that absence IS the skew signal.
"""

from __future__ import annotations

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixtures, not only annotations

from shipyard.preflight import _daemon_version_skew_note


class _FakeConfig:
    """Minimal stand-in for ``shipyard.core.config.Config`` —
    ``_daemon_version_skew_note`` only reads ``state_dir``."""

    def __init__(self, state_dir: str = "/tmp/sy-test") -> None:
        self.state_dir = state_dir


def test_matching_version_returns_none(monkeypatch: pytest.MonkeyPatch) -> None:
    import shipyard

    monkeypatch.setattr(shipyard, "__version__", "0.34.0")
    monkeypatch.setattr(
        "shipyard.daemon.controller.read_daemon_status",
        lambda _: {"shipyard_version": "0.34.0"},
    )
    assert _daemon_version_skew_note(_FakeConfig()) is None


def test_mismatched_version_returns_note(monkeypatch: pytest.MonkeyPatch) -> None:
    import shipyard

    monkeypatch.setattr(shipyard, "__version__", "0.34.0")
    monkeypatch.setattr(
        "shipyard.daemon.controller.read_daemon_status",
        lambda _: {"shipyard_version": "0.29.0"},
    )
    note = _daemon_version_skew_note(_FakeConfig())
    assert note is not None
    assert "v0.29.0" in note
    assert "v0.34.0" in note
    assert "shipyard daemon stop" in note


def test_daemon_not_running_returns_none(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(
        "shipyard.daemon.controller.read_daemon_status",
        lambda _: None,
    )
    assert _daemon_version_skew_note(_FakeConfig()) is None


def test_pre_0_26_daemon_missing_version_field_flags_skew(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Daemons before v0.26.0 don't populate `shipyard_version`. That
    # absence is itself the skew signal.
    monkeypatch.setattr(
        "shipyard.daemon.controller.read_daemon_status",
        lambda _: {"some_other_field": "hello"},
    )
    note = _daemon_version_skew_note(_FakeConfig())
    assert note is not None
    assert "pre" in note.lower() or "predates" in note.lower()
    assert "shipyard daemon stop" in note


def test_read_daemon_status_exception_returns_none(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    """Fail-quiet contract: a flaky daemon-status probe must never
    turn a real preflight error into an aggregate error."""

    def _raiser(_: object) -> dict:
        raise RuntimeError("simulated: daemon socket wedged")

    monkeypatch.setattr(
        "shipyard.daemon.controller.read_daemon_status", _raiser
    )
    assert _daemon_version_skew_note(_FakeConfig()) is None
