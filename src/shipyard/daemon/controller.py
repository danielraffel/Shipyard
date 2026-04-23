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
from typing import TYPE_CHECKING, Any

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
    from datetime import datetime
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
        self._webhook_port: int | None = None
        self._webhook_secret: str | None = None
        self._ipc_server: IPCServer | None = None
        self._tunnel = TailscaleFunnelBackend()
        self._registrar = Registrar(config.state_dir)
        self._stop_event = asyncio.Event()
        self._tunnel_supervisor_task: asyncio.Task[None] | None = None
        # Rolling 5-min dedupe set for X-GitHub-Delivery IDs. GitHub
        # retries failed deliveries with the same ID; without dedupe a
        # retry would re-broadcast an already-seen event, causing double
        # re-evaluation in every waiter.
        self._seen_delivery_ids: dict[str, float] = {}

    async def start(self) -> None:
        """Bring the daemon up.

        Ordering (changed in v0.26.3 — don't revert to the pre-IPC
        sequence without reading shipyard#26):

          1. Acquire the PID lock.
          2. Load the webhook secret.
          3. Bind the webhook HTTP server on localhost (needs a port
             so the tunnel can target it).
          4. **Start the IPC server immediately.** Subscribers (the
             GUI, `shipyard wait`, `shipyard watch --follow`) can now
             connect for ship-state-list queries and live status
             reads regardless of tunnel state. Before this change,
             any Tailscale hiccup at startup killed the whole daemon
             and the GUI fell back to polling with no recovery until
             a manual restart.
          5. Spawn the tunnel supervisor as a background task. It
             retries the Tailscale probe forever (capped backoff),
             registers webhooks when the tunnel becomes ready, and
             watches for tunnel loss mid-session.
          6. Spawn the reconcile loop (unchanged).

        `start()` only raises on truly-fatal startup errors: PID
        lock contention, port bind failures, or socket setup
        failures. Tunnel-related failures no longer take the daemon
        down — they surface through `_build_status_snapshot` so
        subscribers see an accurate tunnel state.
        """
        self._paths.ensure_dirs()
        self._acquire_lock()

        # Resolve secret (keychain on macOS, file on Linux).
        self._webhook_secret = secrets_mod.load_or_create(self._config.state_dir)

        # Bind webhook server → port for the tunnel to target. Even
        # if the tunnel never comes up, the port sits idle and
        # harmless.
        self._webhook_server = WebhookServer(
            _make_delivery_handler(self._webhook_secret, self)
        )
        self._webhook_port = self._webhook_server.start()

        # IPC server comes up here, NOT after the tunnel. See the
        # docstring above for the rationale.
        self._ipc_server = IPCServer(
            socket_path=self._paths.socket_file,
            status_provider=self._build_status_snapshot,
            on_stop_request=self._request_stop,
            ship_state_list_provider=self._build_ship_state_list,
        )
        await self._ipc_server.start()

        # Tunnel supervisor: brings up Funnel (retrying forever with
        # capped backoff), registers webhooks when it's up, watches
        # for mid-session loss.
        self._tunnel_supervisor_task = asyncio.create_task(
            self._tunnel_supervisor_loop()
        )

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
        # Fire the event so a concurrent run() and the tunnel
        # supervisor both observe the stop.
        self._stop_event.set()

        # Cancel the tunnel supervisor before IPC shutdown so it
        # doesn't try to push a status update into a closed server.
        if self._tunnel_supervisor_task is not None:
            self._tunnel_supervisor_task.cancel()
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await self._tunnel_supervisor_task
            self._tunnel_supervisor_task = None

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

    # Capped exponential backoff for tunnel retry. Starts aggressive
    # for fast recovery from tailscaled warmup / DERP fallback, caps
    # at 5min so a tailnet that's genuinely offline for hours doesn't
    # spam the probe (still retries forever, just rarely).
    _TUNNEL_RETRY_BACKOFFS: tuple[float, ...] = (
        2.0, 6.0, 15.0, 30.0, 60.0, 120.0, 300.0,
    )
    # Once the tunnel is up, verify cadence. A tailnet that drops mid-
    # session (sleep/wake, VPN switch, admin panel change) is detected
    # within this window and the supervisor kicks a re-establish.
    _TUNNEL_VERIFY_INTERVAL_SECS: float = 30.0

    async def _tunnel_supervisor_loop(self) -> None:
        """Bring the tunnel up, keep it up, never let it kill the daemon.

        Three phases run in sequence inside an outer while-not-stopped
        loop:

          * **Bring-up.** Probe + start the tunnel. On failure, wait
            `_TUNNEL_RETRY_BACKOFFS[i]` seconds (capped) and retry.
            `_stop_event` preempts the sleep so shutdown is prompt.
          * **Register webhooks.** Idempotent; runs every time the
            tunnel comes up (new tunnel → same Tailscale DNS name →
            existing hook still valid; the registrar dedupes).
          * **Watch.** Verify every
            `_TUNNEL_VERIFY_INTERVAL_SECS`. If the verify fails, flip
            tunnel state to None (so IPC status reports the loss) and
            loop back to bring-up.

        The daemon never exits because of tunnel trouble. IPC stays
        up, `shipyard wait` + GUI fast-path keeps working on the
        local store, and live webhook delivery resumes automatically
        whenever the tunnel recovers.
        """
        # The outer `except Exception` used to log-and-return, which
        # ended the supervisor task after the first unexpected
        # exception. That silently broke self-healing on any non-
        # Tunnel error (#179). We now log and *restart* the inner
        # loop after a short backoff so the supervisor genuinely
        # never gives up short of `_stop_event`.
        #
        # ``crash_attempt`` escalates the restart backoff on
        # consecutive crashes, but resets to 0 whenever the tunnel
        # successfully comes up (see #183): without that, a few
        # isolated one-off failures accumulated forever and pinned
        # future restarts to the 300s max, so later transient issues
        # took much longer to self-heal than intended. Resetting on
        # successful bring-up scopes the backoff to "how bad is *this*
        # crash streak," not "how many crashes has this daemon ever
        # seen across its lifetime."
        crash_attempt = 0
        while not self._stop_event.is_set():
            try:
                while not self._stop_event.is_set():
                    tunnel_info = await self._bring_up_tunnel()
                    if tunnel_info is None:
                        return  # stopped during bring-up

                    self._state.tunnel = tunnel_info
                    self._state.tunnel_verified_at = time.time()
                    logger.info("tunnel ready: %s", tunnel_info.public_url)
                    # Fresh recovery — stop compounding backoff from
                    # pre-recovery crashes. If we crash again later,
                    # backoff starts over at the first bucket.
                    crash_attempt = 0
                    await self._register_webhooks(tunnel_info)

                    # Watch loop: periodically re-verify. When verify
                    # fails, fall through to the outer loop which
                    # restarts bring-up.
                    await self._watch_tunnel()
                    if self._stop_event.is_set():
                        return
                    logger.warning(
                        "tunnel lost mid-session; re-establishing "
                        "(URL was %s)",
                        tunnel_info.public_url,
                    )
                    self._state.tunnel = None
                    self._state.tunnel_verified_at = None
                return
            except asyncio.CancelledError:  # pragma: no cover — shutdown
                raise
            except Exception as exc:  # noqa: BLE001 — keep supervisor alive
                wait_secs = self._TUNNEL_RETRY_BACKOFFS[
                    min(crash_attempt, len(self._TUNNEL_RETRY_BACKOFFS) - 1)
                ]
                crash_attempt += 1
                logger.error(
                    "tunnel supervisor hit unexpected exception (%s): %s "
                    "— restarting loop in %.0fs",
                    type(exc).__name__, exc, wait_secs,
                    exc_info=True,
                )
                try:
                    await asyncio.wait_for(
                        self._stop_event.wait(), timeout=wait_secs
                    )
                    return  # stop requested during backoff
                except TimeoutError:
                    continue

    async def _bring_up_tunnel(self) -> TunnelInfo | None:
        """Retry tunnel.start indefinitely (capped backoff) until it
        succeeds or stop is requested.

        Transient-failure surface has to be wider than the Tunnel-
        specific error classes. `TailscaleFunnelBackend.start` shells
        out via ``asyncio.create_subprocess_exec``, which can raise
        bare ``OSError`` (e.g. `ENOENT` when the `tailscale` binary is
        momentarily gone during a package update), ``FileNotFoundError``
        (same condition, different Python surface), or
        ``asyncio.TimeoutError`` if probing stalls. Before #179 those
        escaped straight into the supervisor's outer ``except Exception``
        and ended the supervisor task after a single log line — the
        daemon kept running but never attempted tunnel recovery again,
        silently breaking self-healing. We retry all three.
        """
        attempt = 0
        assert self._webhook_port is not None  # start() invariant
        while not self._stop_event.is_set():
            try:
                return await self._tunnel.start(self._webhook_port)
            except (
                TunnelNotReadyError,
                TunnelStartError,
                OSError,
                asyncio.TimeoutError,
            ) as exc:
                wait_secs = self._TUNNEL_RETRY_BACKOFFS[
                    min(attempt, len(self._TUNNEL_RETRY_BACKOFFS) - 1)
                ]
                attempt += 1
                logger.info(
                    "tunnel bring-up attempt %d failed (%s): %s "
                    "(retrying in %.0fs)",
                    attempt, type(exc).__name__, exc, wait_secs,
                )
                # Sleep preempted by stop_event so shutdown doesn't
                # wait out the full backoff.
                try:
                    await asyncio.wait_for(
                        self._stop_event.wait(), timeout=wait_secs
                    )
                    return None  # stop requested
                except TimeoutError:
                    continue
        return None

    async def _register_webhooks(self, tunnel_info: TunnelInfo) -> None:
        """Idempotent webhook registration. Safe to call on every
        tunnel bring-up — Registrar reuses existing hook IDs."""
        assert self._webhook_secret is not None
        public_url = tunnel_info.public_url.rstrip("/") + "/webhook"
        for repo in self._config.repos:
            try:
                await self._registrar.ensure_registered(
                    repo, public_url, self._webhook_secret
                )
            except RegistrarError as exc:
                logger.error("failed to register %s: %s", repo, exc)

    async def _watch_tunnel(self) -> None:
        """Poll `tunnel.verify` every `_TUNNEL_VERIFY_INTERVAL_SECS`
        and return once the verification fails or stop is requested."""
        assert self._webhook_port is not None
        while not self._stop_event.is_set():
            try:
                await asyncio.wait_for(
                    self._stop_event.wait(),
                    timeout=self._TUNNEL_VERIFY_INTERVAL_SECS,
                )
                return  # stop requested
            except TimeoutError:
                pass
            try:
                ok = await self._tunnel.verify(self._webhook_port)
            except Exception as exc:  # noqa: BLE001 — verify must never crash the loop
                logger.warning("tunnel verify raised: %s", exc)
                return
            if not ok:
                return

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


RECONCILE_TERMINAL_RUN_STATUSES = frozenset(
    {"completed", "passed", "failed", "cancelled", "canceled"}
)
"""Run statuses that mean "this check has reached a settled state."

Shared with `_is_aged_terminal` below. States whose runs are ALL in
this set AND whose `updated_at` is past the fresh window are skipped
by reconcile — cuts the daemon's gh-API budget on machines with a
long shipyard history. See task #22."""

RECONCILE_FRESH_WINDOW_SECONDS = 3600
"""Grace period after a ship-state's last update during which it's
still reconciled even if all runs are terminal. Covers CI re-runs
that complete quickly after a failure — updated_at still reflects
the recent activity, so reconcile picks up the transition. Past
this window, terminal states are treated as settled and skipped."""

RECONCILE_FORCED_WINDOW_SECONDS = 86400
"""Safety net on top of the fresh-window skip (#176): even if a
ship-state is aged-terminal and we'd normally skip it, every ~24h
we do a single forced reconcile to catch the "missed webhook after
aging" scenario. Without this, a daemon outage or a Tailscale-funnel
drop between the aged-terminal boundary and a late CI transition
left the state permanently un-healable (the webhook that would have
refreshed ``updated_at`` never landed, so the aging clock never
reset). One API call per PR per day preserves the rate-limit budget
while restoring the "eventually correct" self-healing guarantee."""

_LAST_FORCED_RECONCILE: dict[int, datetime] = {}
"""In-memory bookkeeping for the forced-reconcile window. Keyed by
PR number; value is the UTC timestamp of the last forced reconcile.
Cleared on daemon restart (startup re-reconciles everything as
normal drift heal, so losing this is safe — at worst the first tick
after restart runs one extra reconcile per aged-terminal PR)."""


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

    def _is_aged_terminal(state: Any) -> bool:
        """True iff every run is terminal AND the state is past its
        fresh window AND it was force-reconciled within the last
        ``RECONCILE_FORCED_WINDOW_SECONDS``. The forced-window clause
        is the fix for #176: without it, a missed webhook after the
        aging boundary leaves a permanent blind spot because
        ``updated_at`` never refreshes locally. Once a day, we still
        reconcile the aged-terminal state to catch that drift. Same
        bookkeeping decides whether to stamp a fresh
        ``_LAST_FORCED_RECONCILE`` entry, so mutation stays colocated
        with the decision.
        """
        runs = state.dispatched_runs or []
        if not runs:
            evidence = state.evidence_snapshot or {}
            if not evidence:
                return False  # pre-dispatch — always reconcile
            all_terminal = all(
                v in {"pass", "fail", "reused", "skipped"}
                for v in evidence.values()
            )
        else:
            all_terminal = all(
                (r.status or "").lower() in RECONCILE_TERMINAL_RUN_STATUSES
                for r in runs
            )
        if not all_terminal:
            return False
        updated = state.updated_at
        if updated is None:
            return False
        from datetime import datetime as _dt
        from datetime import timezone as _tz

        now = _dt.now(_tz.utc)
        age_secs = (now - updated).total_seconds()
        if age_secs <= RECONCILE_FRESH_WINDOW_SECONDS:
            return False

        # Aged-terminal. Check whether we're due for the 24h forced
        # reconcile. Last-forced timestamp is in-memory, per PR.
        last_forced = _LAST_FORCED_RECONCILE.get(state.pr)
        if (
            last_forced is None
            or (now - last_forced).total_seconds()
            > RECONCILE_FORCED_WINDOW_SECONDS
        ):
            _LAST_FORCED_RECONCILE[state.pr] = now
            return False  # force a reconcile this tick
        return True  # aged-terminal, recently force-reconciled → skip

    def _reconcile_sync() -> tuple[int, list[transition_t]]:
        """Returns (healed count, list of per-target transitions)."""
        import subprocess as _sp

        store = ShipStateStore(state_dir / "ship")
        healed = 0
        transitions: list[transition_t] = []
        skipped_terminal = 0
        for state in store.list_active():
            if _is_aged_terminal(state):
                # Aged-terminal ship — every run is in a terminal
                # status AND the state hasn't been updated in the
                # fresh window. No reconcile drift expected; if CI
                # actually re-runs, the check_run/check_suite
                # webhook refreshes updated_at and the next tick
                # picks it up (no longer aged). Safety net is
                # still intact for the webhook-missed case — the
                # updated_at timestamp advances on every observed
                # change, so any drift resets the aging clock.
                skipped_terminal += 1
                continue
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
        # The skipped-terminal count is intentionally NOT logged —
        # on a machine with 60 terminal ships that'd be a line every
        # 30s with no signal. The relevant view is `shipyard cleanup
        # --ship-state` which surfaces aged-terminal states for
        # optional archiving.
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
