"""Unix socket IPC server for daemon subscribers.

Any ``shipyard`` CLI invocation (including the macOS GUI subprocess)
can connect to the daemon's socket and stream decoded webhook events
as NDJSON. One-line-per-message; newlines inside payloads are not
permitted (JSON encoders produce compact output by default).

Messages from server to client:
    {"type": "hello",      ...}       # on connect
    {"type": "event",      ...}       # webhook delivery
    {"type": "status",     ...}       # response to a status request
    {"type": "goodbye",    ...}       # on graceful server shutdown

Messages from client to server:
    {"type": "subscribe",  "since": "<iso>"?}  # opens event stream
    {"type": "status"}                         # request status snapshot
    {"type": "stop"}                           # ask server to stop (drains then exits)

Slow-subscriber isolation
-------------------------
Each subscriber gets its own bounded ``asyncio.Queue`` fed by
``broadcast_event``. A dedicated writer task drains that queue to the
socket. If the writer can't keep up (client stalled, socket buffer
full), the broadcaster won't block on that subscriber — instead the
slow subscriber is dropped with a ``goodbye`` frame after a bounded
drain timeout. This keeps one confused client from stalling the
reconcile loop or the webhook path.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
from collections import deque
from collections.abc import Awaitable, Callable
from dataclasses import dataclass
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pathlib import Path

logger = logging.getLogger(__name__)

RING_BUFFER_SIZE = 100
SUBSCRIBER_QUEUE_MAX = 64
SUBSCRIBER_DRAIN_TIMEOUT = 2.0

# IPC protocol version. Bump on any client-observable wire change —
# new message type, new required field, shape break. Clients read
# this from the hello frame and can warn / refuse to talk to a daemon
# whose protocol they don't understand.
#
# Version history:
#   1 — initial: hello / event / status / stop / subscribe / goodbye
#   2 — adds ship-state-list request + reply (shipyard#153, v0.25.0)
IPC_PROTOCOL_VERSION = 2


@dataclass
class IPCState:
    """Server-side view of daemon state exposed over the IPC socket."""

    tunnel_backend: str
    tunnel_url: str | None
    tunnel_verified_at: float | None
    subscribers: int
    last_event_at: float | None
    registered_repos: list[str]
    rate_limit: dict[str, object] | None


StatusProvider = Callable[[], IPCState]
StopRequestCallback = Callable[[], Awaitable[None]]
ShipStateListProvider = Callable[[], list[dict[str, object]]]


class _Subscriber:
    """One connected client + the goroutine-style writer draining its queue."""

    def __init__(self, writer: asyncio.StreamWriter) -> None:
        self.writer = writer
        self.queue: asyncio.Queue[dict[str, object] | None] = asyncio.Queue(
            maxsize=SUBSCRIBER_QUEUE_MAX
        )
        self.writer_task: asyncio.Task[None] | None = None
        self.alive = True


class IPCServer:
    """Owns the ``daemon.sock`` listener + fans out events.

    Ring buffer holds the most recent ``RING_BUFFER_SIZE`` events so a
    subscriber that connects mid-session still sees a short history.
    """

    def __init__(
        self,
        socket_path: Path,
        status_provider: StatusProvider,
        on_stop_request: StopRequestCallback | None = None,
        ship_state_list_provider: ShipStateListProvider | None = None,
    ) -> None:
        self._socket_path = socket_path
        self._status_provider = status_provider
        self._on_stop_request = on_stop_request
        self._ship_state_list_provider = ship_state_list_provider
        self._server: asyncio.AbstractServer | None = None
        self._subscribers: set[_Subscriber] = set()
        self._ring: deque[dict[str, object]] = deque(maxlen=RING_BUFFER_SIZE)
        self._lock = asyncio.Lock()

    async def start(self) -> None:
        self._socket_path.parent.mkdir(parents=True, exist_ok=True)
        # Clean up a stale socket from a previous crashed run.
        if self._socket_path.exists() or self._socket_path.is_symlink():
            with contextlib.suppress(OSError):
                self._socket_path.unlink()
        self._server = await asyncio.start_unix_server(
            self._handle_client, path=str(self._socket_path)
        )
        # 600 so other users on a shared box can't subscribe.
        os.chmod(self._socket_path, 0o600)
        logger.info("ipc server listening at %s", self._socket_path)

    async def stop(self) -> None:
        # Say goodbye to active subscribers so they don't hang on the
        # next read and can reconnect cleanly on restart.
        async with self._lock:
            subscribers = list(self._subscribers)
            self._subscribers.clear()
        for sub in subscribers:
            await self._drop_subscriber(sub, reason="server-stop")
        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
            self._server = None
        if self._socket_path.exists() or self._socket_path.is_symlink():
            with contextlib.suppress(OSError):
                self._socket_path.unlink()

    async def broadcast_event(self, event: dict[str, object]) -> None:
        """Append to ring buffer + fan out to every connected subscriber.

        Each subscriber has its own bounded queue. If the queue is full
        the subscriber is dropped so a stalled client can't back-pressure
        the daemon's event loop. The drop itself runs after the lock is
        released to avoid holding it during a goodbye write.
        """
        async with self._lock:
            self._ring.append(event)
            targets = list(self._subscribers)
        message = {"type": "event", **event}
        to_drop: list[_Subscriber] = []
        for sub in targets:
            if not sub.alive:
                continue
            try:
                sub.queue.put_nowait(message)
            except asyncio.QueueFull:
                to_drop.append(sub)
        for sub in to_drop:
            await self._drop_subscriber(sub, reason="slow-subscriber")

    def subscriber_count(self) -> int:
        return len(self._subscribers)

    # --- internals -------------------------------------------------

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        sub = _Subscriber(writer)
        sub.writer_task = asyncio.create_task(self._writer_loop(sub))
        # Send a hello so clients can verify protocol compatibility
        # before doing anything else. Goes through the queue so the
        # writer loop fully owns the socket.
        #
        # `shipyard_version` lets clients detect a stale daemon (long-
        # running process that's older than the on-disk CLI binary).
        # That's the specific pothole that ate most of 2026-04-22:
        # the GUI sent a new request type, the old daemon silently
        # ignored it, no one noticed. With the version in hand the
        # GUI (and `shipyard doctor`) can warn + recommend a restart.
        from shipyard import __version__ as _sy_version

        await sub.queue.put(
            {
                "type": "hello",
                "protocol": IPC_PROTOCOL_VERSION,
                "shipyard_version": _sy_version,
            }
        )
        try:
            while not reader.at_eof():
                line = await reader.readline()
                if not line:
                    break
                try:
                    msg = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if not isinstance(msg, dict):
                    continue
                await self._handle_message(msg, sub)
        finally:
            async with self._lock:
                self._subscribers.discard(sub)
            await self._drop_subscriber(sub, reason="client-disconnect")

    async def _handle_message(
        self, msg: dict[str, object], sub: _Subscriber
    ) -> None:
        msg_type = msg.get("type")
        if msg_type == "subscribe":
            async with self._lock:
                self._subscribers.add(sub)
                backlog = list(self._ring)
            for past in backlog:
                # Ring-buffer replays go through the same queue so
                # ordering is preserved relative to live events.
                try:
                    sub.queue.put_nowait({"type": "event", **past})
                except asyncio.QueueFull:
                    await self._drop_subscriber(sub, reason="slow-subscriber")
                    return
        elif msg_type == "status":
            state = self._status_provider()
            from shipyard import __version__ as _sy_version

            await sub.queue.put(
                {
                    "type": "status",
                    "tunnel": {
                        "backend": state.tunnel_backend,
                        "url": state.tunnel_url,
                        "verified_at": state.tunnel_verified_at,
                    },
                    "subscribers": state.subscribers,
                    "last_event_at": state.last_event_at,
                    "registered_repos": state.registered_repos,
                    "rate_limit": state.rate_limit,
                    # Expose the daemon's own version + protocol so
                    # `shipyard doctor` / the GUI can flag drift
                    # against the on-disk CLI binary. Clients not
                    # aware of these fields ignore them (additive).
                    "shipyard_version": _sy_version,
                    "protocol": IPC_PROTOCOL_VERSION,
                },
            )
        elif msg_type == "stop":
            if self._on_stop_request is not None:
                await self._on_stop_request()
        elif msg_type == "ship-state-list":
            # Daemon-side shortcut for `shipyard --json ship-state list`.
            # Subscribers (primarily the macOS GUI) would otherwise pay
            # the PyInstaller CLI cold-start tax (~5-6s) on every
            # 7s poll tick; serving the same JSON directly from the
            # in-daemon `ShipStateStore` returns in milliseconds.
            # See shipyard#153.
            if self._ship_state_list_provider is None:
                await sub.queue.put(
                    {"type": "ship-state-list", "states": []}
                )
            else:
                states = self._ship_state_list_provider()
                await sub.queue.put(
                    {"type": "ship-state-list", "states": states}
                )

    async def _writer_loop(self, sub: _Subscriber) -> None:
        """Drains the subscriber's queue into the socket.

        A ``None`` sentinel stops the loop (sent by ``_drop_subscriber``).
        Per-message drain is bounded by ``SUBSCRIBER_DRAIN_TIMEOUT`` — a
        subscriber that stalls ``writer.drain()`` past that window is
        dropped so it can't block the broadcaster indirectly.
        """
        while sub.alive:
            msg = await sub.queue.get()
            if msg is None:
                return
            try:
                payload = (
                    json.dumps(msg, separators=(",", ":")) + "\n"
                ).encode()
                sub.writer.write(payload)
                await asyncio.wait_for(
                    sub.writer.drain(), timeout=SUBSCRIBER_DRAIN_TIMEOUT
                )
            except (TimeoutError, ConnectionError, OSError):
                # Slow or dead subscriber — mark so the broadcaster
                # stops feeding the queue and drop cleanly.
                sub.alive = False
                async with self._lock:
                    self._subscribers.discard(sub)
                await _send_goodbye_and_close(sub.writer)
                return

    async def _drop_subscriber(self, sub: _Subscriber, reason: str) -> None:
        if not sub.alive:
            return
        sub.alive = False
        # Wake the writer loop so it exits; then send the goodbye
        # frame directly (queue may already be drained/closed).
        with contextlib.suppress(asyncio.QueueFull):
            sub.queue.put_nowait(None)
        if sub.writer_task is not None:
            with contextlib.suppress(asyncio.CancelledError, Exception):
                await asyncio.wait_for(sub.writer_task, timeout=0.5)
        await _send_goodbye_and_close(sub.writer)
        if reason == "slow-subscriber":
            logger.warning("ipc: dropped slow subscriber")


async def _send_goodbye_and_close(writer: asyncio.StreamWriter) -> None:
    with contextlib.suppress(ConnectionError, OSError):
        writer.write(
            (
                json.dumps({"type": "goodbye"}, separators=(",", ":")) + "\n"
            ).encode()
        )
        with contextlib.suppress(TimeoutError):
            await asyncio.wait_for(
                writer.drain(), timeout=SUBSCRIBER_DRAIN_TIMEOUT
            )
    with contextlib.suppress(ConnectionError, OSError):
        writer.close()
        with contextlib.suppress(TimeoutError):
            await asyncio.wait_for(writer.wait_closed(), timeout=1.0)
