"""Reconcile-driven IPC emission: every healed target → one event.

Integration-ish: drive `_reconcile_all_active_ships` with a fake
`gh` (monkeypatched subprocess) + a real ship-state file; assert the
daemon broadcasts a `reconcile_healed` event per healed target.
"""

from __future__ import annotations

import asyncio
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

import pytest

from shipyard.core.ship_state import DispatchedRun, ShipState, ShipStateStore
from shipyard.daemon.controller import Daemon, DaemonConfig, _reconcile_all_active_ships

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="reconcile shells out to gh; keep tests on macOS/Linux",
)


class _FakeIPC:
    def __init__(self) -> None:
        self.broadcasts: list[dict] = []

    async def broadcast_event(self, event: dict) -> None:
        self.broadcasts.append(event)


def _make_state(repo: str, pr: int, target_status: str) -> ShipState:
    now = datetime.now(timezone.utc)
    return ShipState(
        pr=pr,
        repo=repo,
        branch="feat/x",
        base_branch="main",
        head_sha="abc123",
        policy_signature="sig",
        dispatched_runs=[
            DispatchedRun(
                target="Linux",
                provider="namespace",
                run_id="100",
                status=target_status,
                started_at=now,
                updated_at=now,
            )
        ],
        evidence_snapshot={"Linux": "pending"},
        attempt=1,
    )


def test_reconcile_emits_event_when_target_transitions(monkeypatch) -> None:
    """A ship-state file shows Linux=failed, GitHub says SUCCESS; the
    reconcile loop should heal the state AND broadcast a
    reconcile_healed event with the right transition."""

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-reconcile-") as tmp:
            state_dir = Path(tmp)
            store = ShipStateStore(state_dir / "ship")
            state = _make_state("o/r", 151, "failed")
            store.save(state)

            fake_stdout = (
                '{"statusCheckRollup": [{"name": "Linux",'
                ' "state": "COMPLETED", "conclusion": "SUCCESS"}]}'
            )

            class _FakeCompleted:
                def __init__(self, stdout: str) -> None:
                    self.stdout = stdout
                    self.returncode = 0

            def fake_run(*args, **kwargs):  # type: ignore[no-untyped-def]
                return _FakeCompleted(fake_stdout)

            monkeypatch.setattr(subprocess, "run", fake_run)

            daemon = Daemon(DaemonConfig(state_dir=state_dir, repos=[]))
            fake_ipc = _FakeIPC()
            daemon._ipc_server = fake_ipc  # type: ignore[assignment]

            await _reconcile_all_active_ships(state_dir, daemon)

            assert len(fake_ipc.broadcasts) == 1
            broadcast = fake_ipc.broadcasts[0]
            assert broadcast["kind"] == "reconcile_healed"
            assert broadcast["payload"]["pr"] == 151
            assert broadcast["payload"]["target"] == "Linux"
            assert broadcast["payload"]["from_status"] == "failed"
            assert broadcast["payload"]["to_status"] == "completed"

    asyncio.run(run())


def test_reconcile_no_transition_emits_nothing(monkeypatch) -> None:
    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-reconcile-") as tmp:
            state_dir = Path(tmp)
            store = ShipStateStore(state_dir / "ship")
            state = _make_state("o/r", 151, "completed")
            store.save(state)

            fake_stdout = (
                '{"statusCheckRollup": [{"name": "Linux",'
                ' "state": "COMPLETED", "conclusion": "SUCCESS"}]}'
            )

            class _FakeCompleted:
                def __init__(self, stdout: str) -> None:
                    self.stdout = stdout
                    self.returncode = 0

            monkeypatch.setattr(
                subprocess, "run", lambda *a, **k: _FakeCompleted(fake_stdout)
            )

            daemon = Daemon(DaemonConfig(state_dir=state_dir, repos=[]))
            fake_ipc = _FakeIPC()
            daemon._ipc_server = fake_ipc  # type: ignore[assignment]

            await _reconcile_all_active_ships(state_dir, daemon)
            assert fake_ipc.broadcasts == []

    asyncio.run(run())
