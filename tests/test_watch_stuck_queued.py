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
    _watch_signature,
)
from shipyard.core.ship_state import DispatchedRun, ShipState


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


# -- Codex P1 follow-up on #206 -------------------------------------
# `watch --follow` only re-renders when `_watch_signature` changes.
# Stuck-queued is time-based: a run that stays `queued` with no
# transition will never flip the signature under the pre-fix
# composition (which folded only status/phase/heartbeat). The exact
# failure mode #190 is supposed to catch — saturated Namespace
# queue, no other state change — thus stayed silent under --follow.
# These tests lock in that the signature now flips at threshold
# crossing and stays stable on either side of it.

def _ship_state_with(runs: list[DispatchedRun]) -> ShipState:
    return ShipState(
        pr=1,
        repo="x/y",
        branch="b",
        base_branch="main",
        head_sha="abc",
        policy_signature="p",
        dispatched_runs=runs,
    )


def test_signature_flips_when_run_crosses_stuck_threshold(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # One queued run, 400s old. With threshold=600s it's not stuck
    # (sq=0); with threshold=300s it is (sq=1). No other state
    # changes between the two captures — only the threshold moved —
    # but the signature MUST differ so watch --follow re-emits.
    run = _run("queued", 400)
    state = _ship_state_with([run])

    monkeypatch.setenv("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS", "600")
    sig_before = _watch_signature(state)

    monkeypatch.setenv("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS", "300")
    sig_after = _watch_signature(state)

    assert sig_before != sig_after, (
        "signature must flip on threshold crossing; "
        f"got identical {sig_before!r}"
    )
    assert "sq=0" in sig_before
    assert "sq=1" in sig_after


def test_signature_stable_when_both_runs_stay_sub_threshold(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Two sub-threshold captures in a row must produce the same
    # signature — we only want re-emit on the crossing event, not
    # on every watch loop. Threshold held high on both sides.
    run = _run("queued", 100)
    state = _ship_state_with([run])

    monkeypatch.setenv("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS", "600")
    sig_a = _watch_signature(state)
    sig_b = _watch_signature(state)

    assert sig_a == sig_b
    assert "sq=0" in sig_a


def test_signature_stable_after_both_runs_already_stuck(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Symmetric to the sub-threshold case: once a run is stuck,
    # successive captures at the same threshold must not churn the
    # signature. Otherwise --follow would re-emit on every poll
    # for a long-stuck run, which is the opposite failure.
    run = _run("queued", 900)
    state = _ship_state_with([run])

    monkeypatch.setenv("SHIPYARD_STUCK_QUEUED_THRESHOLD_SECS", "300")
    sig_a = _watch_signature(state)
    sig_b = _watch_signature(state)

    assert sig_a == sig_b
    assert "sq=1" in sig_a
