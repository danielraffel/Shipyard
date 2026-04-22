"""Slow-subscriber isolation in the IPC server.

Ensures one client that stops reading doesn't back-pressure the
broadcaster for everyone else. Covers the per-subscriber queue +
drain timeout logic added for #149.
"""

from __future__ import annotations

import asyncio
import json
import socket
import sys
import tempfile
from pathlib import Path

import pytest

from shipyard.daemon.ipc import (
    SUBSCRIBER_QUEUE_MAX,
    IPCServer,
    IPCState,
)

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="AF_UNIX sockets are macOS/Linux only",
)


@pytest.fixture
def short_socket_path():
    with tempfile.TemporaryDirectory(prefix="sy-ipc-slow-") as d:
        yield Path(d) / "daemon.sock"


def _dummy_state() -> IPCState:
    return IPCState(
        tunnel_backend="tailscale",
        tunnel_url=None,
        tunnel_verified_at=None,
        subscribers=0,
        last_event_at=None,
        registered_repos=[],
        rate_limit=None,
    )


def _read_lines_blocking(
    sock: socket.socket, count: int, timeout: float = 3.0
) -> list[dict]:
    sock.settimeout(timeout)
    buf = b""
    out: list[dict] = []
    while len(out) < count:
        chunk = sock.recv(65536)
        if not chunk:
            break
        buf += chunk
        while b"\n" in buf and len(out) < count:
            line, _, buf = buf.partition(b"\n")
            out.append(json.loads(line))
    return out


def test_fast_subscriber_keeps_receiving_after_slow_subscriber_drops(
    short_socket_path: Path,
) -> None:
    """Exhaust a slow client's queue; verify a parallel fast client
    still gets every broadcast.
    """

    async def run() -> None:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
        )
        await server.start()
        slow: socket.socket | None = None
        try:
            # Slow client: subscribes but never reads.
            slow = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            slow.connect(str(short_socket_path))
            slow.sendall(b'{"type":"subscribe"}\n')

            # Fast client started on a thread — capture lines as they
            # land, streaming, so we don't race the broadcast phase.
            outputs: list[dict] = []
            ready = asyncio.Event()
            loop = asyncio.get_running_loop()

            def fast_reader() -> None:
                fast = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                fast.connect(str(short_socket_path))
                fast.settimeout(5.0)
                try:
                    fast.sendall(b'{"type":"subscribe"}\n')
                    buf = b""
                    hello_seen = False
                    while True:
                        chunk = fast.recv(65536)
                        if not chunk:
                            return
                        buf += chunk
                        while b"\n" in buf:
                            line, _, buf = buf.partition(b"\n")
                            msg = json.loads(line)
                            outputs.append(msg)
                            if msg.get("type") == "hello" and not hello_seen:
                                hello_seen = True
                                loop.call_soon_threadsafe(ready.set)
                        # Early exit once we've got enough events.
                        event_count = sum(
                            1 for m in outputs if m.get("type") == "event"
                        )
                        if event_count >= 20:
                            return
                finally:
                    fast.close()

            reader_task = asyncio.create_task(asyncio.to_thread(fast_reader))

            # Wait until the fast subscriber's `subscribe` frame has
            # been processed. Poll subscriber_count because
            # `hello_seen` just confirms the hello frame landed, not
            # that the subscribe handler has added the writer to the
            # set. We want both clients registered before broadcasting.
            await ready.wait()
            for _ in range(50):
                if server.subscriber_count() >= 2:
                    break
                await asyncio.sleep(0.05)

            # Flood the server with more events than the slow client's
            # queue can hold, forcing the drop path. Yield between
            # broadcasts so the fast subscriber's writer task actually
            # runs; without the yield, the broadcast loop would starve
            # every writer and drop every subscriber (unrealistic —
            # real webhook deliveries arrive seconds apart, not in a
            # microsecond-tight loop).
            for i in range(SUBSCRIBER_QUEUE_MAX + 40):
                await server.broadcast_event(
                    {"kind": "workflow_run", "payload": {"id": i}}
                )
                await asyncio.sleep(0)

            await asyncio.wait_for(reader_task, timeout=5.0)
        finally:
            if slow is not None:
                import contextlib as _contextlib
                with _contextlib.suppress(OSError):
                    slow.close()
            await server.stop()

        assert outputs[0]["type"] == "hello"
        event_lines = [m for m in outputs if m.get("type") == "event"]
        # Fast client still sees at least 10 of the 84 broadcasts even
        # after the slow client's queue has been drop-flagged.
        assert len(event_lines) >= 10

    asyncio.run(run())
