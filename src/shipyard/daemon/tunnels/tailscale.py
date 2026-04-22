"""Tailscale Funnel tunnel backend.

Mirrors ``TunnelController.swift`` + ``TailscaleProbe.swift`` —
``tailscale funnel reset`` + ``tailscale funnel --bg <port>`` with a
post-start verification loop. Without the verify-and-retry the Swift
version hit a race where ``--bg`` returned exit 0 without the daemon
actually persisting the proxy config; we do the same safety dance
here.
"""

from __future__ import annotations

import asyncio

from shipyard.daemon import tailscale as probe_mod
from shipyard.daemon.tunnels.base import TunnelInfo, TunnelNotReadyError, TunnelStartError

# tailscaled can return a partial / not-yet-populated status when it's
# warming up, restoring a profile, rebuilding its cap map after a
# control-plane reconnect, or mid-DERP-fallback. A single probe that
# lands in that window returns `funnel_permitted=False` even on a node
# that absolutely has Funnel in its ACLs. Retrying with short backoff
# recovers in every case observed so far. Total worst-case wait before
# giving up: ~20s, which is long enough to cross most tailscaled
# transients but short enough that a genuinely-misconfigured tailnet
# still fails fast.
_PROBE_BACKOFFS_SECS: tuple[float, ...] = (2.0, 6.0, 12.0)


async def _probe_with_retry() -> probe_mod.TailscaleStatus:
    status = await asyncio.to_thread(probe_mod.probe)
    if status.is_ready:
        return status
    for delay in _PROBE_BACKOFFS_SECS:
        await asyncio.sleep(delay)
        status = await asyncio.to_thread(probe_mod.probe)
        if status.is_ready:
            return status
    return status


class TailscaleFunnelBackend:
    name = "tailscale"

    def __init__(self) -> None:
        self._binary: str | None = None
        self._configured_port: int | None = None

    async def detect(self) -> bool:
        self._binary = probe_mod.resolve_binary()
        if self._binary is None:
            return False
        status = await _probe_with_retry()
        return status.is_ready

    async def start(self, local_port: int) -> TunnelInfo:
        status = await _probe_with_retry()
        if not status.is_ready or status.funnel_url is None:
            raise TunnelNotReadyError(
                "Tailscale Funnel isn't ready after retries: "
                f"backend={status.backend_state!r} "
                f"funnel_permitted={status.funnel_permitted}"
            )
        self._binary = status.binary_path

        binary = self._binary
        assert binary is not None  # is_ready above guarantees this

        # Reset first so a previous launch's mapping doesn't win.
        # Without this, `funnel --bg <newport>` is a no-op when `/` is
        # already claimed by the old port. Swift hit this exact bug.
        await self._run([binary, "funnel", "reset"])
        # Brief settle — daemon sometimes hasn't applied the reset
        # when the next call lands, and the second call then silently
        # no-ops.
        await asyncio.sleep(0.5)

        last_output = ""
        for attempt in range(1, 4):
            code, output = await self._run(
                [binary, "funnel", "--bg", str(local_port)]
            )
            last_output = output
            if code != 0:
                raise TunnelStartError(f"funnel --bg failed: {output.strip()}")
            if await self._verify_configured(local_port):
                self._configured_port = local_port
                return TunnelInfo(public_url=status.funnel_url, backend=self.name)
            await asyncio.sleep(0.5 * attempt)
        raise TunnelStartError(
            "funnel --bg returned 0 but serve config didn't persist. "
            f"last output: {last_output.strip()}"
        )

    async def stop(self) -> None:
        if self._binary is None:
            self._binary = probe_mod.resolve_binary()
        if self._binary is None:
            return
        await self._run([self._binary, "funnel", "reset"])
        self._configured_port = None

    async def verify(self, local_port: int) -> bool:
        if self._binary is None:
            return False
        return await self._verify_configured(local_port)

    async def _verify_configured(self, local_port: int) -> bool:
        assert self._binary is not None
        _code, output = await self._run([self._binary, "funnel", "status"])
        return f"127.0.0.1:{local_port}" in output

    async def _run(self, argv: list[str]) -> tuple[int, str]:
        proc = await asyncio.create_subprocess_exec(
            *argv,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.STDOUT,
        )
        stdout, _ = await proc.communicate()
        return proc.returncode or 0, (stdout or b"").decode(errors="replace")
