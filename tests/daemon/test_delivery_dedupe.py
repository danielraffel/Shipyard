"""Dedupe + reconcile IPC emission — controller-level behavior.

These tests exercise `_on_delivery` and `broadcast_reconcile_healed`
without requiring a fully-running daemon (no webhook server, no
tunnel, no registrar). We drive the Daemon instance directly.
"""

from __future__ import annotations

import asyncio
import sys
import tempfile
from pathlib import Path

import pytest

from shipyard.daemon import events as events_mod
from shipyard.daemon.controller import DELIVERY_DEDUPE_TTL_SECONDS, Daemon, DaemonConfig

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="AF_UNIX sockets are macOS/Linux only",
)


class _FakeIPC:
    """Captures broadcasts for assertion without listening on a real socket."""

    def __init__(self) -> None:
        self.broadcasts: list[dict] = []

    async def broadcast_event(self, event: dict) -> None:
        self.broadcasts.append(event)


def _make_daemon(tmp: Path) -> Daemon:
    daemon = Daemon(DaemonConfig(state_dir=tmp, repos=[]))
    daemon._ipc_server = _FakeIPC()  # type: ignore[assignment]
    return daemon


def _workflow_run_event() -> events_mod.WebhookEvent:
    return events_mod.WebhookEvent(
        kind="workflow_run",
        workflow_run=events_mod.WorkflowRunPayload(
            action="completed",
            run_id=42,
            repo="o/r",
            head_branch="b",
            head_sha="s",
            status="completed",
            conclusion="success",
            workflow_name="CI",
            html_url=None,
        ),
    )


def test_duplicate_delivery_id_drops_silently() -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-dd-") as tmp:
            daemon = _make_daemon(Path(tmp))
            ipc = daemon._ipc_server
            assert isinstance(ipc, _FakeIPC)
            await daemon._on_delivery(_workflow_run_event(), "delivery-1")
            await daemon._on_delivery(_workflow_run_event(), "delivery-1")
            await daemon._on_delivery(_workflow_run_event(), "delivery-2")
            assert len(ipc.broadcasts) == 2

    asyncio.run(run())


def test_missing_delivery_id_always_broadcasts() -> None:
    """Reconcile-generated events carry no X-GitHub-Delivery; they must
    not be silently deduped."""
    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-dd-") as tmp:
            daemon = _make_daemon(Path(tmp))
            ipc = daemon._ipc_server
            assert isinstance(ipc, _FakeIPC)
            await daemon._on_delivery(_workflow_run_event(), None)
            await daemon._on_delivery(_workflow_run_event(), None)
            assert len(ipc.broadcasts) == 2

    asyncio.run(run())


def test_expired_delivery_id_is_evicted() -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-dd-") as tmp:
            daemon = _make_daemon(Path(tmp))
            ipc = daemon._ipc_server
            assert isinstance(ipc, _FakeIPC)
            # Simulate a delivery from > 5 minutes ago.
            daemon._seen_delivery_ids["expired"] = (
                0.0  # unix epoch — definitely past TTL
            )
            await daemon._on_delivery(_workflow_run_event(), "expired")
            # Broadcasts because the stored timestamp was evicted.
            assert len(ipc.broadcasts) == 1

    asyncio.run(run())


def test_reconcile_healed_event_shape() -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-dd-") as tmp:
            daemon = _make_daemon(Path(tmp))
            ipc = daemon._ipc_server
            assert isinstance(ipc, _FakeIPC)
            await daemon.broadcast_reconcile_healed(
                pr=151,
                repo="o/r",
                target="windows",
                from_status="failed",
                to_status="completed",
            )
            assert len(ipc.broadcasts) == 1
            broadcast = ipc.broadcasts[0]
            assert broadcast["kind"] == "reconcile_healed"
            assert broadcast["payload"] == {
                "pr": 151,
                "repo": "o/r",
                "target": "windows",
                "from_status": "failed",
                "to_status": "completed",
            }

    asyncio.run(run())


def test_dedupe_ttl_constant_is_5_minutes() -> None:
    assert DELIVERY_DEDUPE_TTL_SECONDS == 300.0
