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


DELIVERY_DEDUPE_TTL_SECONDS = 300.0
"""How long a webhook X-GitHub-Delivery ID stays in the dedupe set.

GitHub retries failed deliveries with the same ``X-GitHub-Delivery``
header for up to a few minutes. 5 minutes is comfortably past the
retry window without growing the set unbounded on a high-volume repo."""


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
        # Rolling 5-min dedupe set for X-GitHub-Delivery IDs. GitHub
        # retries failed deliveries with the same ID; without dedupe a
        # retry would re-broadcast an already-seen event, causing double
        # re-evaluation in every waiter.
        self._seen_delivery_ids: dict[str, float] = {}

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
            ship_state_list_provider=self._build_ship_state_list,
        )
        await self._ipc_server.start()

        # Continuously heal ship-state drift against GitHub truth.
        # Webhook events alone can't keep state accurate — re-runs get
        # new run_ids and old events don't match, failed dispatched_run
        # updates can be missed during daemon downtime, manual GH
        # interventions (re-running a failed check, merging via the
        # web UI) bypass the webhook path entirely. Periodic reconcile
        # closes every one of those gaps.
        #
        # Cost: one `gh pr view --json statusCheckRollup` per active PR
        # every 30s. At 5 active PRs that's 600/hr — 12% of the 5000/hr
        # authenticated REST budget. Existing GUI polling already uses
        # ~60/hr per repo, so this is a marginal add.
        #
        # First tick runs immediately so startup drift heals without
        # the 30s initial delay.
        asyncio.create_task(_reconcile_loop(self._config.state_dir, self))

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

    async def _on_delivery(
        self,
        event: events_mod.WebhookEvent,
        delivery_id: str | None = None,
    ) -> None:
        """Called from the HTTP handler thread with a decoded event.

        ``delivery_id`` comes from GitHub's ``X-GitHub-Delivery`` header.
        Seen IDs are dropped silently so that a retried delivery doesn't
        re-broadcast the same event. The header is only visible at HTTP
        receipt time — IPC sees the already-decoded dict and can't
        reconstruct the ID, so dedupe has to sit here.
        """
        self._state.last_event_at = time.time()
        if delivery_id:
            now = time.time()
            cutoff = now - DELIVERY_DEDUPE_TTL_SECONDS
            # Evict expired IDs so the set doesn't grow unbounded.
            stale = [d for d, t in self._seen_delivery_ids.items() if t < cutoff]
            for d in stale:
                del self._seen_delivery_ids[d]
            if delivery_id in self._seen_delivery_ids:
                return
            self._seen_delivery_ids[delivery_id] = now
        if self._ipc_server is not None:
            await self._ipc_server.broadcast_event(event.to_wire())

    async def broadcast_reconcile_healed(
        self,
        *,
        pr: int,
        repo: str,
        target: str,
        from_status: str,
        to_status: str,
    ) -> None:
        """Emit a synthetic ``reconcile_healed`` event over IPC.

        Fired by the reconcile loop when it finds drift between local
        ship-state and GitHub truth. Lets waiters (``shipyard wait``,
        GUI) re-evaluate immediately rather than waiting for the next
        poll tick. Never carries a delivery_id — the dedupe path in
        ``_on_delivery`` intentionally ignores absent IDs.
        """
        if self._ipc_server is None:
            return
        await self._ipc_server.broadcast_event(
            {
                "kind": "reconcile_healed",
                "payload": {
                    "pr": pr,
                    "repo": repo,
                    "target": target,
                    "from_status": from_status,
                    "to_status": to_status,
                },
            }
        )

    def _build_ship_state_list(self) -> list[dict[str, object]]:
        """Return the same JSON shape `shipyard --json ship-state list`
        emits, read directly from the local store.

        Serves the ``{"type":"ship-state-list"}`` IPC request so
        subscribers (the macOS GUI, primarily) don't have to pay the
        PyInstaller cold-start tax on every poll. See shipyard#153.

        Read through the store so we pick up any concurrent writes
        from the ship path. If the read throws (disk glitch, partial
        write), return an empty list rather than crashing the daemon.
        """
        try:
            from shipyard.core.ship_state import ShipStateStore

            store = ShipStateStore(self._config.state_dir / "ship")
            return [s.to_dict() for s in store.list_active()]
        except Exception as exc:  # noqa: BLE001 — never crash daemon
            logger.warning("ship-state-list IPC: %s", exc)
            return []

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
    inline. Pulls ``X-GitHub-Delivery`` out of the headers here because
    it's the only place it's visible — IPC downstream sees only the
    decoded dict.
    """
    loop = asyncio.get_event_loop()

    def handler(headers: dict[str, str], body: bytes) -> HandlerResult:
        if not signature.is_valid(
            body, secret, headers.get("x-hub-signature-256")
        ):
            return HandlerResult.unauthorized()
        event = events_mod.decode(headers.get("x-github-event"), body)
        if event is not None:
            delivery_id = headers.get("x-github-delivery")
            asyncio.run_coroutine_threadsafe(
                daemon._on_delivery(event, delivery_id), loop
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


RECONCILE_INTERVAL_SECONDS = 30
"""How often the daemon re-fetches statusCheckRollup for every active PR.

Short enough to feel near-live for humans watching the GUI, long enough
to keep the REST call count well inside GitHub's 5000/hr budget even
with a dozen active PRs. Tuned for a single-user menu-bar deployment —
lower this if you need real-time freshness, raise it if you have more
active PRs than REST budget tolerates."""


async def _reconcile_loop(state_dir: Path, daemon: Daemon | None = None) -> None:
    """Continuously heal ship-state drift against GitHub truth.

    This is the definitive fix for the class of bugs where:
      * a failed check got re-run on GitHub but the new run_id didn't
        match any dispatched_run entry → event dropped → stale "failed"
      * a manual merge / close on GitHub bypassed our webhook path
      * a daemon outage dropped state-transition events that would
        have resolved the drift

    First tick runs immediately so startup drift heals without a 30s
    initial delay. Subsequent ticks on RECONCILE_INTERVAL_SECONDS.
    Cancellation propagates cleanly through the asyncio.sleep.
    """
    while True:
        try:
            await _reconcile_all_active_ships(state_dir, daemon)
        except asyncio.CancelledError:  # pragma: no cover — shutdown path
            raise
        except Exception as exc:  # noqa: BLE001 — must never crash daemon
            logger.warning("reconcile loop: iteration failed: %s", exc)
        try:
            await asyncio.sleep(RECONCILE_INTERVAL_SECONDS)
        except asyncio.CancelledError:  # pragma: no cover — shutdown path
            return


async def _reconcile_all_active_ships(
    state_dir: Path, daemon: Daemon | None = None
) -> None:
    """Best-effort: for every active ship-state file, pull the current
    CI rollup from GitHub and write back any changes.

    Runs in the asyncio loop but shells out to `gh` via a thread so a
    slow GitHub response doesn't stall the event loop. Errors are
    logged and skipped — reconcile failure must never block daemon
    startup or event processing.

    If ``daemon`` is provided, per-target status transitions are
    published over IPC as synthetic ``reconcile_healed`` events so
    waiters can re-evaluate without waiting for the next poll tick.
    """
    from shipyard.core.ship_state import ShipStateStore
    from shipyard.ship.reconcile import reconcile_ship_state

    # (pr, repo, target, before_status, after_status)
    transition_t = tuple[int, str, str, str, str]

    def _reconcile_sync() -> tuple[int, list[transition_t]]:
        """Returns (healed count, list of per-target transitions)."""
        import subprocess as _sp

        store = ShipStateStore(state_dir / "ship")
        healed = 0
        transitions: list[transition_t] = []
        for state in store.list_active():
            try:
                raw = _sp.run(
                    [
                        "gh", "pr", "view", str(state.pr),
                        "--repo", state.repo,
                        "--json", "statusCheckRollup",
                    ],
                    capture_output=True, text=True, check=True, timeout=20,
                ).stdout
            except (_sp.CalledProcessError, _sp.TimeoutExpired, FileNotFoundError) as exc:
                logger.info(
                    "reconcile: skipped PR #%d (%s): %s",
                    state.pr, state.repo, exc,
                )
                continue
            try:
                rollup = json.loads(raw).get("statusCheckRollup") or []
            except (ValueError, KeyError):
                continue
            prior_statuses = {r.target: r.status for r in state.dispatched_runs}
            new_state, changes = reconcile_ship_state(state, rollup)
            if changes:
                store.save(new_state)
                healed += 1
                logger.info(
                    "reconcile: healed PR #%d — %s",
                    state.pr, "; ".join(changes),
                )
                for run in new_state.dispatched_runs:
                    before = prior_statuses.get(run.target, "")
                    if before and before != run.status:
                        transitions.append(
                            (state.pr, state.repo, run.target, before, run.status)
                        )
        return healed, transitions

    try:
        healed, transitions = await asyncio.to_thread(_reconcile_sync)
        if healed:
            logger.info(
                "reconcile: %d active ship-state(s) updated", healed
            )
        if daemon is not None:
            for pr, repo, target, before, after in transitions:
                await daemon.broadcast_reconcile_healed(
                    pr=pr,
                    repo=repo,
                    target=target,
                    from_status=before,
                    to_status=after,
                )
    except Exception as exc:  # noqa: BLE001 — best-effort, never crash startup
        logger.warning("reconcile iteration failed: %s", exc)
