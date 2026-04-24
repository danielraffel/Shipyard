"""Regression tests for #190 stuck-queued run annotation.

When a dispatched run sits in a queued-family status past the
configured threshold, `shipyard watch` now surfaces that as a
`stuck-queued Nm` marker in human output and `stuck_queued: true`
+ `queued_for_secs: N` fields in JSON mode. Motivated by the
2026-04-23 Namespace Windows saturation (#193) — jobs queued 30+
min with no visible signal.
"""

from __future__ import annotations

from datetime import datetime, timedelta, timezone

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixture

from shipyard.cli import (
    _format_stuck_queued_duration,
    _is_stuck_queued,
    _queued_for_secs,
    _stuck_queued_threshold_secs,
)
from shipyard.core.ship_state import DispatchedRun


def _run(status: str, age_secs: int) -> DispatchedRun:
    now = datetime.now(timezone.utc)
    started = now - timedelta(seconds=age_secs)
    return DispatchedRun(
        target="macos",
        provider="namespace",
        run_id="r1",
        status=status,
        started_at=started,
        updated_at=started,
    )


def test_queued_for_secs_returns_age_for_queued_runs() -> None:
    now = datetime.now(timezone.utc)
    assert _queued_for_secs(_run("queued", 120), now) is not None
    assert _queued_for_secs(_run("queued", 120), now) >= 119


def test_queued_for_secs_returns_none_for_non_queued() -> None:
    now = datetime.now(timezone.utc)
    for status in ("in_progress", "completed", "success", "failed"):
        assert _queued_for_secs(_run(status, 500), now) is None


def test_is_stuck_queued_fires_past_threshold() -> None:
    now = datetime.now(timezone.utc)
    # 5-min-queued run is stuck at threshold 4m but not at 6m.
    run = _run("queued", 300)
    assert _is_stuck_queued(run, now, threshold=240.0) is True
    assert _is_stuck_queued(run, now, threshold=360.0) is False


def test_is_stuck_queued_does_not_fire_for_in_progress() -> None:
    now = datetime.now(timezone.utc)
    # Even a very old in-progress run is not "stuck queued" — only
    # queued-family statuses qualify. Different failure mode from
    # "heartbeat went silent" (which has its own `stale` marker).
    run = _run("in_progress", 10_000)
    assert _is_stuck_queued(run, now, threshold=60.0) is False


def test_threshold_default_is_300s() -> None:
    # Default derived from #190: long enough to not flag warm-up
    # queue, tight enough to catch 15-30 min scheduler stalls.
    assert _stuck_queued_threshold_secs() == 300.0


def test_threshold_respects_env_override(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS", "60")
    assert _stuck_queued_threshold_secs() == 60.0


def test_threshold_ignores_malformed_env(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    monkeypatch.setenv("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS", "not-a-number")
    assert _stuck_queued_threshold_secs() == 300.0


def test_format_duration_shapes() -> None:
    assert _format_stuck_queued_duration(45) == "45s"
    assert _format_stuck_queued_duration(125) == "2m"
    assert _format_stuck_queued_duration(3900) == "1h5m"


def test_waiting_and_pending_statuses_also_qualify() -> None:
    # GitHub Actions uses "queued" but the underlying workflow run
    # state model also emits "pending"/"waiting" in edge cases. All
    # three should be treated the same by the stuck-queued check.
    now = datetime.now(timezone.utc)
    for status in ("queued", "pending", "waiting"):
        assert _queued_for_secs(_run(status, 500), now) is not None
        assert _is_stuck_queued(_run(status, 500), now, threshold=60.0) is True
