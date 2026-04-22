"""Transient-Funnel-unavailable retry for the Tailscale tunnel backend.

tailscaled briefly reports `funnel_permitted=False` / empty
BackendState when it's warming up, restoring a profile, or
reconnecting to the control plane. A one-shot probe at daemon
startup that lands in that window used to kill the daemon with
"Tailscale Funnel isn't ready" and leave the GUI stuck in polling.
These tests pin the retry-with-backoff recovery path.
"""

from __future__ import annotations

import asyncio

import pytest

from shipyard.daemon import tailscale as probe_mod
from shipyard.daemon.tunnels import tailscale as tunnel_mod
from shipyard.daemon.tunnels.base import TunnelNotReadyError


def _status(*, ready: bool) -> probe_mod.TailscaleStatus:
    return probe_mod.TailscaleStatus(
        binary_path="/Applications/Tailscale.app/Contents/MacOS/Tailscale" if ready else None,
        backend_state="Running" if ready else None,
        dns_name="test.example.ts.net." if ready else None,
        funnel_permitted=ready,
    )


def test_probe_retries_and_eventually_succeeds(monkeypatch: pytest.MonkeyPatch) -> None:
    """Two not-ready reads in a row, then a ready one. Caller should see ready."""
    calls = iter([_status(ready=False), _status(ready=False), _status(ready=True)])

    def fake_probe() -> probe_mod.TailscaleStatus:
        return next(calls)

    monkeypatch.setattr(probe_mod, "probe", fake_probe)
    monkeypatch.setattr(tunnel_mod, "_PROBE_BACKOFFS_SECS", (0.0, 0.0, 0.0))

    status = asyncio.run(tunnel_mod._probe_with_retry())
    assert status.is_ready is True


def test_probe_gives_up_after_all_retries(monkeypatch: pytest.MonkeyPatch) -> None:
    """Every probe returns not-ready → caller gets the last (not-ready)
    status, `start` then raises TunnelNotReadyError."""
    monkeypatch.setattr(probe_mod, "probe", lambda: _status(ready=False))
    monkeypatch.setattr(tunnel_mod, "_PROBE_BACKOFFS_SECS", (0.0, 0.0, 0.0))

    async def go() -> None:
        backend = tunnel_mod.TailscaleFunnelBackend()
        with pytest.raises(TunnelNotReadyError) as excinfo:
            await backend.start(local_port=1234)
        # Error message should flag that the retries ran (so a user
        # reading the log knows we tried, not that we gave up
        # immediately on one bad read).
        assert "after retries" in str(excinfo.value)

    asyncio.run(go())


def test_probe_first_attempt_ready_skips_retry(monkeypatch: pytest.MonkeyPatch) -> None:
    """Happy path: one probe, one call, no sleeps."""
    call_count = {"n": 0}

    def fake_probe() -> probe_mod.TailscaleStatus:
        call_count["n"] += 1
        return _status(ready=True)

    monkeypatch.setattr(probe_mod, "probe", fake_probe)

    sleep_calls: list[float] = []

    async def fake_sleep(d: float) -> None:
        sleep_calls.append(d)

    monkeypatch.setattr(asyncio, "sleep", fake_sleep)

    status = asyncio.run(tunnel_mod._probe_with_retry())
    assert status.is_ready is True
    assert call_count["n"] == 1
    assert sleep_calls == [], "no backoff should fire when first probe succeeds"
