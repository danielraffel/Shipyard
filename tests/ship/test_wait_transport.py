"""Transport orchestration for `shipyard wait`.

Covers the four documented scenarios:

* daemon-happy-path (snapshot miss → event drives match).
* daemon-down-with-fallback (polling matches later).
* daemon-down-no-fallback + snapshot already matches → exit 0.
* daemon-down-no-fallback + snapshot misses → exit 6.
"""

from __future__ import annotations

import asyncio
import sys
import tempfile
from pathlib import Path

import pytest

from shipyard.daemon.ipc import IPCServer, IPCState
from shipyard.ship.wait import TruthResult
from shipyard.ship.wait_transport import (
    WaitOutcome,
    pr_event_filter,
    wait_for_condition,
)

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="AF_UNIX sockets are macOS/Linux only",
)


@pytest.fixture
def short_socket_path():
    with tempfile.TemporaryDirectory(prefix="sy-wait-") as d:
        yield Path(d) / "daemon.sock"


def _dummy_state() -> IPCState:
    return IPCState(
        tunnel_backend="tailscale",
        tunnel_url=None,
        tunnel_verified_at=None,
        subscribers=0,
        last_event_at=None,
        registered_repos=[],
        rate_limit=None,
    )


def test_snapshot_already_matches_exits_immediately(short_socket_path: Path) -> None:
    """No daemon needed when the first snapshot already matches."""
    calls: list[int] = []

    def fetch() -> dict:
        calls.append(1)
        return {"status": "completed"}

    def evaluator(snap: dict | None) -> TruthResult:
        return TruthResult(matched=True, observed=snap or {})

    outcome = wait_for_condition(
        evaluator=evaluator,
        fetch_snapshot=fetch,
        event_filter=lambda e: True,
        timeout_seconds=5.0,
        poll_interval_seconds=0.2,
        no_fallback=True,  # should NOT trigger — snapshot matches first
        socket_path=short_socket_path,  # doesn't exist; daemon unreachable
    )
    assert outcome.matched is True
    assert outcome.transport == "polling"
    assert outcome.fallback_disabled_hit is False
    assert len(calls) == 1


def test_no_fallback_and_snapshot_miss_returns_fallback_disabled(
    short_socket_path: Path,
) -> None:
    def fetch() -> dict:
        return {"status": "in_progress"}

    def evaluator(snap: dict | None) -> TruthResult:
        return TruthResult(matched=False, observed=snap or {})

    outcome = wait_for_condition(
        evaluator=evaluator,
        fetch_snapshot=fetch,
        event_filter=lambda e: True,
        timeout_seconds=5.0,
        poll_interval_seconds=0.2,
        no_fallback=True,
        socket_path=short_socket_path,
    )
    assert outcome.matched is False
    assert outcome.fallback_disabled_hit is True
    assert outcome.transport == "polling"


def test_polling_fallback_matches_after_a_few_polls(short_socket_path: Path) -> None:
    counter = {"n": 0}

    def fetch() -> dict:
        counter["n"] += 1
        return {"status": "completed" if counter["n"] >= 3 else "in_progress"}

    def evaluator(snap: dict | None) -> TruthResult:
        return TruthResult(
            matched=bool(snap and snap.get("status") == "completed"),
            observed=snap or {},
        )

    outcome = wait_for_condition(
        evaluator=evaluator,
        fetch_snapshot=fetch,
        event_filter=lambda e: True,
        timeout_seconds=5.0,
        poll_interval_seconds=0.05,
        no_fallback=False,
        socket_path=short_socket_path,
    )
    assert outcome.matched is True
    assert counter["n"] >= 3


def test_daemon_happy_path_live_event_triggers_re_evaluation(
    short_socket_path: Path,
) -> None:
    """Subscribe to a real IPCServer, ensure the waiter wakes on a
    broadcasted event and re-evaluates the snapshot to a match."""

    counter = {"n": 0}

    def fetch() -> dict:
        counter["n"] += 1
        # First call (snapshot) misses; subsequent calls match.
        return {"status": "completed" if counter["n"] > 1 else "pending"}

    def evaluator(snap: dict | None) -> TruthResult:
        return TruthResult(
            matched=bool(snap and snap.get("status") == "completed"),
            observed=snap or {},
        )

    async def run() -> WaitOutcome:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
        )
        await server.start()
        try:
            async def driver() -> None:
                # Give the waiter time to subscribe + snapshot.
                await asyncio.sleep(0.3)
                await server.broadcast_event(
                    {
                        "kind": "pull_request",
                        "payload": {"number": 42},
                    }
                )

            driver_task = asyncio.create_task(driver())

            def _run() -> WaitOutcome:
                return wait_for_condition(
                    evaluator=evaluator,
                    fetch_snapshot=fetch,
                    event_filter=pr_event_filter(42, "o/r"),
                    timeout_seconds=3.0,
                    poll_interval_seconds=0.05,
                    no_fallback=False,
                    socket_path=short_socket_path,
                )

            outcome = await asyncio.to_thread(_run)
            await driver_task
            return outcome
        finally:
            await server.stop()

    outcome = asyncio.run(run())
    assert outcome.matched is True
    assert outcome.transport == "daemon"
    # One for snapshot + one (or more) from the event-driven re-fetch.
    assert counter["n"] >= 2


def test_pr_event_filter_drops_unrelated_events() -> None:
    f = pr_event_filter(151, "o/r")
    assert f({"kind": "pull_request", "payload": {"number": 151}}) is True
    assert f({"kind": "pull_request", "payload": {"number": 9999}}) is False
    assert (
        f(
            {
                "kind": "check_run",
                "payload": {"pull_request_numbers": [151], "repo": "o/r"},
            }
        )
        is True
    )
    assert (
        f({"kind": "reconcile_healed", "payload": {"pr": 151, "repo": "o/r"}})
        is True
    )
    assert (
        f({"kind": "workflow_job", "payload": {"repo": "o/r"}})
        is False
    )
