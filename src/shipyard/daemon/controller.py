"""Daemon orchestrator — glues the pieces together.

Responsibilities:
    * Acquire a single-instance lock.
    * Decide whether the Tailscale backend is ready.
    * Spin up the webhook HTTP server + IPC server.
    * Bring up the tunnel, register GitHub hooks.
    * Fan out decoded events to IPC subscribers.
    * Teardown cleanly on stop (unregister hooks, reset funnel,
      remove PID + socket).

State directory layout::

    <state_dir>/daemon/
        daemon.pid
        daemon.sock
        registrations.json
        webhook-secret   (Linux only; macOS uses Keychain)
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import signal
import time
from dataclasses import dataclass
from typing import TYPE_CHECKING

from shipyard.daemon import events as events_mod
from shipyard.daemon import secrets as secrets_mod
from shipyard.daemon import signature
from shipyard.daemon.ipc import IPCServer, IPCState
from shipyard.daemon.registrar import Registrar, RegistrarError
from shipyard.daemon.server import HandlerResult, WebhookServer
from shipyard.daemon.tunnels.base import TunnelInfo, TunnelNotReadyError, TunnelStartError
from shipyard.daemon.tunnels.tailscale import TailscaleFunnelBackend

if TYPE_CHECKING:
    from collections.abc import Callable
    from pathlib import Path

logger = logging.getLogger(__name__)


class DaemonAlreadyRunningError(Exception):
    """Another daemon instance is holding the PID lock."""


@dataclass
class DaemonConfig:
    state_dir: Path
    repos: list[str]
    """Which repos to register webhooks on. Normally derived from
    shipyard ship-state at startup; callers may pass an explicit list
    for testing."""


@dataclass
class _RuntimeState:
    tunnel: TunnelInfo | None = None
    tunnel_verified_at: float | None = None
    last_event_at: float | None = None


class Daemon:
    """The long-running process.

    Typical lifecycle::

        daemon = Daemon(config)
        await daemon.start()   # acquires lock, brings up tunnel, etc.
        await daemon.run()     # blocks until stopped
        await daemon.stop()    # graceful teardown
    """

    def __init__(self, config: DaemonConfig) -> None:
        self._config = config
        self._paths = _DaemonPaths(config.state_dir)
        self._pid_file: Path = self._paths.pid_file
        self._state = _RuntimeState()
        self._webhook_server: WebhookServer | None = None
        self._ipc_server: IPCServer | None = None
        self._tunnel = TailscaleFunnelBackend()
        self._registrar = Registrar(config.state_dir)
        self._stop_event = asyncio.Event()

    async def start(self) -> None:
        self._paths.ensure_dirs()
        self._acquire_lock()

        # Resolve secret (keychain on macOS, file on Linux).
        secret = secrets_mod.load_or_create(self._config.state_dir)

        # Bring the HTTP server up first — need the bound port for
        # the tunnel backend.
        self._webhook_server = WebhookServer(_make_delivery_handler(secret, self))
        port = self._webhook_server.start()

        # Bring the tunnel up.
        try:
            tunnel_info = await self._tunnel.start(port)
        except (TunnelNotReadyError, TunnelStartError):
            # If the tunnel can't come up, the daemon is still useful
            # as a local-only subscribe host (future), but for v1 the
            # whole point is webhook delivery. Fail fast.
            self._webhook_server.stop()
            self._release_lock()
            raise
        self._state.tunnel = tunnel_info
        self._state.tunnel_verified_at = time.time()

        # Register webhooks. URL always ends in /webhook — the server
        # also accepts / for legacy compatibility.
        public_url = tunnel_info.public_url.rstrip("/") + "/webhook"
        for repo in self._config.repos:
            try:
                await self._registrar.ensure_registered(repo, public_url, secret)
            except RegistrarError as exc:
                logger.error("failed to register %s: %s", repo, exc)

        # Last: the IPC server (so subscribers can connect once
        # everything else is live).
        self._ipc_server = IPCServer(
            socket_path=self._paths.socket_file,
            status_provider=self._build_status_snapshot,
            on_stop_request=self._request_stop,
        )
        await self._ipc_server.start()

    async def run(self) -> None:
        """Block until a stop is requested."""
        loop = asyncio.get_running_loop()
        for sig in (signal.SIGINT, signal.SIGTERM):
            # Windows doesn't support add_signal_handler — suppress so
            # CI on Windows can at least reach the wait path (even
            # though the daemon as a whole isn't Windows-supported).
            with contextlib.suppress(NotImplementedError):
                loop.add_signal_handler(sig, lambda: self._stop_event.set())
        await self._stop_event.wait()

    async def stop(self) -> None:
        # Fire the event so a concurrent run() returns.
        self._stop_event.set()

        if self._ipc_server is not None:
            await self._ipc_server.stop()
            self._ipc_server = None

        # Unregister hooks before tearing the tunnel down so the
        # webhook endpoint is still live when the DELETE calls land.
        try:
            await self._registrar.unregister_all()
        except RegistrarError as exc:
            logger.error("unregister_all failed: %s", exc)

        await self._tunnel.stop()

        if self._webhook_server is not None:
            self._webhook_server.stop()
            self._webhook_server = None

        self._release_lock()

    async def _request_stop(self) -> None:
        self._stop_event.set()

    async def _on_delivery(self, event: events_mod.WebhookEvent) -> None:
        """Called from the HTTP handler thread with a decoded event."""
        self._state.last_event_at = time.time()
        if self._ipc_server is not None:
            await self._ipc_server.broadcast_event(event.to_wire())

    def _build_status_snapshot(self) -> IPCState:
        return IPCState(
            tunnel_backend=self._tunnel.name,
            tunnel_url=self._state.tunnel.public_url if self._state.tunnel else None,
            tunnel_verified_at=self._state.tunnel_verified_at,
            subscribers=self._ipc_server.subscriber_count() if self._ipc_server else 0,
            last_event_at=self._state.last_event_at,
            registered_repos=sorted(self._registrar.all().keys()),
            rate_limit=None,
        )

    # --- PID lock --------------------------------------------------

    def _acquire_lock(self) -> None:
        if self._pid_file.exists():
            try:
                pid = int(self._pid_file.read_text(encoding="utf-8").strip())
            except (OSError, ValueError):
                pid = 0
            if pid > 0 and _pid_alive(pid):
                raise DaemonAlreadyRunningError(
                    f"daemon already running (pid {pid}); "
                    "run `shipyard daemon stop` first"
                )
            # Stale file — caller crashed.
            with contextlib.suppress(OSError):
                self._pid_file.unlink()
        self._pid_file.write_text(str(os.getpid()), encoding="utf-8")

    def _release_lock(self) -> None:
        if self._pid_file.exists():
            with contextlib.suppress(OSError):
                self._pid_file.unlink()


@dataclass(frozen=True)
class _DaemonPaths:
    state_dir: Path

    @property
    def root(self) -> Path:
        return self.state_dir / "daemon"

    @property
    def pid_file(self) -> Path:
        return self.root / "daemon.pid"

    @property
    def socket_file(self) -> Path:
        return self.root / "daemon.sock"

    def ensure_dirs(self) -> None:
        self.root.mkdir(parents=True, exist_ok=True)


def _make_delivery_handler(
    secret: str, daemon: Daemon
) -> Callable[[dict[str, str], bytes], HandlerResult]:
    """Produce a callback suitable for ``WebhookServer``.

    The HTTP server runs on a background thread; we schedule the
    async ``_on_delivery`` onto the event loop rather than running it
    inline.
    """
    loop = asyncio.get_event_loop()

    def handler(headers: dict[str, str], body: bytes) -> HandlerResult:
        if not signature.is_valid(
            body, secret, headers.get("x-hub-signature-256")
        ):
            return HandlerResult.unauthorized()
        event = events_mod.decode(headers.get("x-github-event"), body)
        if event is not None:
            asyncio.run_coroutine_threadsafe(
                daemon._on_delivery(event), loop
            )
        return HandlerResult.ok()

    return handler


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def read_daemon_status(state_dir: Path) -> dict[str, object] | None:
    """Query the running daemon over its IPC socket. Returns None if
    the daemon isn't running / reachable.

    Protocol (see `ipc.py`):
      1. Server sends `{"type":"hello","protocol":1}` on connect.
      2. Client sends `{"type":"status"}`.
      3. Server sends `{"type":"status", ...}` back.

    The pre-0.22.5 version of this function read only until the first
    newline and then searched for a status line in `buf.splitlines()`.
    That first newline is the hello, so this always returned None while
    the daemon was happily running — `shipyard daemon status` printed
    "daemon is not running." even though the process was alive.

    Fixed here: read lines until we either see the `type==status`
    reply or the socket times out / closes.
    """
    import socket

    sock_path = state_dir / "daemon" / "daemon.sock"
    if not sock_path.exists():
        return None
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
            client.settimeout(2.0)
            client.connect(str(sock_path))
            client.sendall(b'{"type":"status"}\n')
            buf = b""
            while True:
                chunk = client.recv(65536)
                if not chunk:
                    break
                buf += chunk
                # Drain any complete lines we've accumulated and check
                # each for the status reply. Exit as soon as we find it
                # so we don't block on further reads.
                while b"\n" in buf:
                    line, _, buf = buf.partition(b"\n")
                    try:
                        obj = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    if isinstance(obj, dict) and obj.get("type") == "status":
                        return obj
    except (TimeoutError, OSError):
        return None
    return None
