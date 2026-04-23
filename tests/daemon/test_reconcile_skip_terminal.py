"""Daemon reconcile skips aged-terminal PRs.

Reconcile fetches `gh pr view --json statusCheckRollup` for every
active ship-state every 30s. With 60+ states the budget blows past
5000/hr. Most of those 60 are long-settled (terminal runs, last
touched days ago). Skip reconcile for states that are BOTH:

  - all dispatched_runs in a terminal status (or evidence_snapshot
    entries all terminal for legacy states with empty runs), AND
  - `updated_at` older than 1 hour (past the fresh window).

Fresh terminal states still get reconciled — preserves the
"reconcile heals a CI re-run that just completed" path that
test_reconcile_emits_events.py depends on. The fresh window is
short enough that webhook events reset it on any real state change,
keeping the safety net intact.

Task #22.
"""

from __future__ import annotations

import asyncio
import subprocess
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

import pytest

from shipyard.core.ship_state import DispatchedRun, ShipState, ShipStateStore
from shipyard.daemon.controller import _reconcile_all_active_ships

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="reconcile shells out to gh; macOS/Linux only",
)


def _ship(*, pr: int, runs: list[tuple[str, str]], age_secs: int = 0) -> ShipState:
    """Build a ShipState. `age_secs` backdates `updated_at` so tests
    can control fresh-vs-aged."""
    now = datetime.now(timezone.utc)
    when = now - timedelta(seconds=age_secs)
    dispatched = [
        DispatchedRun(
            target=t,
            provider="namespace",
            run_id=f"r-{pr}-{t}",
            status=s,
            started_at=when,
            updated_at=when,
        )
        for t, s in runs
    ]
    return ShipState(
        pr=pr,
        repo="o/r",
        branch=f"feat/{pr}",
        base_branch="main",
        head_sha="abc123",
        policy_signature="sig",
        dispatched_runs=dispatched,
        evidence_snapshot={t: "pass" for t, _ in runs},
        attempt=1,
        created_at=when,
        updated_at=when,
    )


def test_aged_terminal_ships_skip_gh_view(monkeypatch) -> None:
    """Three aged-terminal ships + one still running. Steady-state
    reconcile (after the forced-window stamp is already fresh) hits
    `gh pr view` exactly once — for the running one. Each aged ship
    has updated_at > 1hr in the past.

    Pre-populates ``_LAST_FORCED_RECONCILE`` so the 24h forced-window
    doesn't fire during this tick (see #176); that path is covered
    by ``test_aged_terminal_forced_reconcile_runs_once_per_day``.
    """
    from shipyard.daemon import controller

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-skip-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            store.save(_ship(
                pr=1, runs=[("mac", "completed")], age_secs=7200,
            ))
            store.save(_ship(
                pr=2, runs=[("mac", "failed")], age_secs=7200,
            ))
            store.save(_ship(
                pr=3, runs=[("mac", "in_progress")], age_secs=7200,
            ))

            gh_calls: list[list[str]] = []

            class _Completed:
                stdout = '{"statusCheckRollup": []}'
                returncode = 0

            def fake_run(cmd, *a, **kw):
                gh_calls.append(cmd)
                return _Completed()

            # Pretend both aged-terminal PRs had their forced-window
            # reconcile moments ago so this tick can exercise the
            # pure-skip branch. The forced-window branch is covered
            # separately in the dedicated test.
            now = datetime.now(timezone.utc)
            controller._LAST_FORCED_RECONCILE[1] = now
            controller._LAST_FORCED_RECONCILE[2] = now

            monkeypatch.setattr(subprocess, "run", fake_run)
            await _reconcile_all_active_ships(Path(tmp), daemon=None)

            assert len(gh_calls) == 1
            assert "3" in gh_calls[0]

    asyncio.run(run())


def test_fresh_terminal_ships_still_reconciled(monkeypatch) -> None:
    """A terminal ship whose updated_at is recent (< 1hr) must STILL
    be reconciled — this is the CI-re-run recovery scenario. The
    existing `test_reconcile_emits_event_when_target_transitions`
    test pins this behavior."""

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-skip-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            # Fresh terminal (age=0).
            store.save(_ship(pr=1, runs=[("mac", "failed")]))

            gh_calls: list[list[str]] = []

            class _Completed:
                stdout = '{"statusCheckRollup": []}'
                returncode = 0

            monkeypatch.setattr(
                subprocess, "run",
                lambda c, *a, **kw: gh_calls.append(c) or _Completed(),
            )
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert len(gh_calls) == 1, (
                "fresh terminal states must still be reconciled — "
                "catches CI re-runs that just completed"
            )

    asyncio.run(run())


def test_pre_dispatch_ship_always_reconciled(monkeypatch) -> None:
    """Pre-dispatch ship (empty dispatched_runs + empty evidence)
    must NEVER be skipped regardless of age — we haven't even
    observed a first rollup yet."""

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-skip-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            s = _ship(pr=42, runs=[], age_secs=86400)
            s.evidence_snapshot = {}
            store.save(s)

            gh_calls: list[list[str]] = []

            class _Completed:
                stdout = '{"statusCheckRollup": []}'
                returncode = 0

            monkeypatch.setattr(
                subprocess, "run",
                lambda c, *a, **kw: gh_calls.append(c) or _Completed(),
            )
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert len(gh_calls) == 1

    asyncio.run(run())


def test_aged_terminal_forced_reconcile_runs_once_per_day(monkeypatch) -> None:
    """#176 regression: aged-terminal states must reconcile at least
    once per RECONCILE_FORCED_WINDOW_SECONDS even if we'd normally
    skip them, so a missed-webhook scenario can't leave them
    permanently un-healable. First tick force-reconciles; a second
    tick seconds later skips; a tick past the forced window
    force-reconciles again."""
    from shipyard.daemon import controller

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-forced-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            # Aged-terminal: 2h old, all runs completed.
            store.save(_ship(
                pr=99, runs=[("mac", "completed")], age_secs=7200,
            ))

            gh_calls: list[list[str]] = []

            class _Completed:
                stdout = '{"statusCheckRollup": []}'
                returncode = 0

            monkeypatch.setattr(
                subprocess, "run",
                lambda c, *a, **kw: gh_calls.append(c) or _Completed(),
            )
            # Reset the in-memory bookkeeping so this test's forcing
            # logic is deterministic regardless of earlier tests.
            controller._LAST_FORCED_RECONCILE.clear()

            # Tick 1: never force-reconciled → force this tick.
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert len(gh_calls) == 1, (
                "aged-terminal with no prior forced reconcile must "
                "trigger one forced reconcile"
            )

            # Tick 2: just force-reconciled → skip.
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert len(gh_calls) == 1, (
                "aged-terminal force-reconciled moments ago must be "
                "skipped — budget is per-day, not per-tick"
            )

            # Advance the last-forced stamp by >24h and tick again.
            from datetime import datetime as _dt
            from datetime import timedelta as _td
            from datetime import timezone as _tz
            controller._LAST_FORCED_RECONCILE[99] = (
                _dt.now(_tz.utc) - _td(seconds=90_000)
            )
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert len(gh_calls) == 2, (
                "aged-terminal last force-reconciled >24h ago must "
                "trigger another forced reconcile"
            )

    asyncio.run(run())


def test_forced_reconcile_failure_does_not_consume_budget(monkeypatch) -> None:
    """#182 regression: if the forced reconcile's `gh pr view` call
    fails (transient CLI error, timeout, missing `gh`), we must NOT
    stamp ``_LAST_FORCED_RECONCILE``. Stamping on failure would
    consume the 24h forced-reconcile budget and leave the state
    un-healable for the next day — the exact permanent blind spot
    the forced window was supposed to close."""
    from shipyard.daemon import controller

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-fail-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            store.save(_ship(
                pr=55, runs=[("mac", "completed")], age_secs=7200,
            ))

            # Fail with a CalledProcessError on every gh pr view call.
            def failing_run(cmd, *a, **kw):
                raise subprocess.CalledProcessError(
                    returncode=1, cmd=cmd, stderr="simulated gh failure"
                )

            monkeypatch.setattr(subprocess, "run", failing_run)
            controller._LAST_FORCED_RECONCILE.clear()

            # First tick: aged-terminal with no prior forced reconcile
            # → attempt a forced reconcile → gh call fails → stamp
            # must NOT be set.
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert 55 not in controller._LAST_FORCED_RECONCILE, (
                "forced reconcile that failed at gh pr view must NOT "
                "consume the 24h budget"
            )

            # Second tick (moments later): still no stamp, so the
            # forced path tries again immediately. Budget untouched.
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert 55 not in controller._LAST_FORCED_RECONCILE

    asyncio.run(run())


def test_successful_reconcile_stamps_forced_window(monkeypatch) -> None:
    """Sanity: when the forced reconcile's `gh pr view` succeeds,
    ``_LAST_FORCED_RECONCILE`` gets stamped so the next 24h of
    ticks correctly skip the state."""
    from shipyard.daemon import controller

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-ok-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            store.save(_ship(
                pr=56, runs=[("mac", "completed")], age_secs=7200,
            ))

            class _Completed:
                stdout = '{"statusCheckRollup": []}'
                returncode = 0

            monkeypatch.setattr(
                subprocess, "run",
                lambda c, *a, **kw: _Completed(),
            )
            controller._LAST_FORCED_RECONCILE.clear()

            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert 56 in controller._LAST_FORCED_RECONCILE, (
                "successful forced reconcile must stamp the "
                "forced-window timestamp"
            )

    asyncio.run(run())


def test_aged_evidence_only_ship_is_skipped(monkeypatch) -> None:
    """Legacy ship-state layout: only evidence_snapshot populated.
    Aged + all evidence entries terminal → skip (in steady-state,
    i.e. after the forced-window stamp from #176 is already fresh).
    """
    from shipyard.daemon import controller

    async def run() -> None:
        with tempfile.TemporaryDirectory(prefix="sy-skip-") as tmp:
            store = ShipStateStore(Path(tmp) / "ship")
            s = _ship(pr=7, runs=[], age_secs=7200)
            s.evidence_snapshot = {"mac": "pass", "linux": "fail"}
            store.save(s)

            gh_calls: list[list[str]] = []

            class _Completed:
                stdout = '{"statusCheckRollup": []}'
                returncode = 0

            # See the sibling test above: pre-stamp the forced-window
            # timestamp so this tick exercises the steady-state skip
            # branch.
            controller._LAST_FORCED_RECONCILE[7] = datetime.now(timezone.utc)

            monkeypatch.setattr(
                subprocess, "run",
                lambda c, *a, **kw: gh_calls.append(c) or _Completed(),
            )
            await _reconcile_all_active_ships(Path(tmp), daemon=None)
            assert gh_calls == []

    asyncio.run(run())
