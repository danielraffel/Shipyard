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
    ) -> None:
        self._socket_path = socket_path
        self._status_provider = status_provider
        self._on_stop_request = on_stop_request
        self._server: asyncio.AbstractServer | None = None
        self._subscribers: set[asyncio.StreamWriter] = set()
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
        for writer in subscribers:
            await _send_safe(writer, {"type": "goodbye"})
            await _close_safe(writer)
        if self._server is not None:
            self._server.close()
            await self._server.wait_closed()
            self._server = None
        if self._socket_path.exists() or self._socket_path.is_symlink():
            with contextlib.suppress(OSError):
                self._socket_path.unlink()

    async def broadcast_event(self, event: dict[str, object]) -> None:
        """Append to ring buffer + fan out to every connected subscriber."""
        async with self._lock:
            self._ring.append(event)
            targets = list(self._subscribers)
        message = {"type": "event", **event}
        for writer in targets:
            await _send_safe(writer, message)

    def subscriber_count(self) -> int:
        return len(self._subscribers)

    # --- internals -------------------------------------------------

    async def _handle_client(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        # Send a hello so clients can verify protocol compatibility
        # before doing anything else.
        await _send_safe(writer, {"type": "hello", "protocol": 1})
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
                await self._handle_message(msg, writer)
        finally:
            async with self._lock:
                self._subscribers.discard(writer)
            await _close_safe(writer)

    async def _handle_message(
        self, msg: dict[str, object], writer: asyncio.StreamWriter
    ) -> None:
        msg_type = msg.get("type")
        if msg_type == "subscribe":
            async with self._lock:
                self._subscribers.add(writer)
                backlog = list(self._ring)
            for past in backlog:
                await _send_safe(writer, {"type": "event", **past})
        elif msg_type == "status":
            state = self._status_provider()
            await _send_safe(
                writer,
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
                },
            )
        elif msg_type == "stop":
            if self._on_stop_request is not None:
                await self._on_stop_request()


async def _send_safe(writer: asyncio.StreamWriter, message: dict[str, object]) -> None:
    try:
        writer.write((json.dumps(message, separators=(",", ":")) + "\n").encode())
        await writer.drain()
    except (ConnectionError, OSError):
        pass


async def _close_safe(writer: asyncio.StreamWriter) -> None:
    try:
        writer.close()
        await writer.wait_closed()
    except (ConnectionError, OSError):
        pass
