"""Unit tests for the ship progress heartbeat helper.

See task #29: ``shipyard ship`` used to go silent during validation
for minutes on long CI matrices, so agents polled ``gh pr view``
out-of-band to check liveness. The helper prints one line every
``_HEARTBEAT_MIN_INTERVAL_SECS`` or on phase change.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import pytest

from shipyard.cli import _HEARTBEAT_MIN_INTERVAL_SECS, _maybe_emit_progress_heartbeat


@pytest.fixture
def captured(monkeypatch: pytest.MonkeyPatch) -> list[tuple[str, str]]:
    lines: list[tuple[str, str]] = []

    def _fake_render(msg: str, style: str = "") -> None:
        lines.append((msg, style))

    monkeypatch.setattr("shipyard.cli.render_message", _fake_render)
    return lines


def test_first_call_prints(captured: list[tuple[str, str]]) -> None:
    state: dict = {"last_heartbeat_print": 0.0, "last_printed_phase": None}
    _maybe_emit_progress_heartbeat(
        progress_state=state,
        target_name="mac",
        target_backend="local",
        phase="running",
        job_started=None,
    )
    assert len(captured) == 1
    msg, style = captured[0]
    assert "mac" in msg and "local" in msg and "running" in msg
    assert style == "dim"


def test_same_phase_within_interval_is_throttled(
    captured: list[tuple[str, str]], monkeypatch: pytest.MonkeyPatch
) -> None:
    clock = {"t": 1000.0}
    monkeypatch.setattr("time.monotonic", lambda: clock["t"])

    state: dict = {"last_heartbeat_print": 0.0, "last_printed_phase": None}
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="running", job_started=None,
    )
    assert len(captured) == 1

    # Still well inside the 30s window → no second print.
    clock["t"] += _HEARTBEAT_MIN_INTERVAL_SECS - 1
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="running", job_started=None,
    )
    assert len(captured) == 1


def test_phase_change_always_prints(
    captured: list[tuple[str, str]], monkeypatch: pytest.MonkeyPatch
) -> None:
    clock = {"t": 1000.0}
    monkeypatch.setattr("time.monotonic", lambda: clock["t"])

    state: dict = {"last_heartbeat_print": 0.0, "last_printed_phase": None}
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="running", job_started=None,
    )
    # One second later — phase flipped; must print even though the
    # interval hasn't elapsed.
    clock["t"] += 1
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="building", job_started=None,
    )
    assert len(captured) == 2
    assert "building" in captured[1][0]


def test_interval_elapsed_prints_again(
    captured: list[tuple[str, str]], monkeypatch: pytest.MonkeyPatch
) -> None:
    clock = {"t": 1000.0}
    monkeypatch.setattr("time.monotonic", lambda: clock["t"])

    state: dict = {"last_heartbeat_print": 0.0, "last_printed_phase": None}
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="running", job_started=None,
    )
    clock["t"] += _HEARTBEAT_MIN_INTERVAL_SECS + 0.1
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="running", job_started=None,
    )
    assert len(captured) == 2


def test_elapsed_renders_minutes_and_seconds(
    captured: list[tuple[str, str]],
) -> None:
    state: dict = {"last_heartbeat_print": 0.0, "last_printed_phase": None}
    started = datetime.now(timezone.utc) - timedelta(seconds=125)
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="namespace",
        phase="running", job_started=started,
    )
    assert "2m" in captured[0][0]


def test_elapsed_renders_seconds_under_a_minute(
    captured: list[tuple[str, str]],
) -> None:
    state: dict = {"last_heartbeat_print": 0.0, "last_printed_phase": None}
    started = datetime.now(timezone.utc) - timedelta(seconds=5)
    _maybe_emit_progress_heartbeat(
        progress_state=state, target_name="mac", target_backend="local",
        phase="running", job_started=started,
    )
    # Low precision so we don't flake on slow CI — just assert it has
    # an "s" suffix in parens.
    assert "s)" in captured[0][0]
