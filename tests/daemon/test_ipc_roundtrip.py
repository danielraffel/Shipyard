"""End-to-end IPC round-trip: broadcast_event → subscriber sees it.

Exercises the Unix socket server + ring buffer + subscribe semantics
without needing a running daemon. Each test is its own event loop so
pytest-asyncio isn't required.
"""

from __future__ import annotations

import asyncio
import json
import socket
import sys
import tempfile
from pathlib import Path

import pytest

from shipyard.daemon.ipc import IPCServer, IPCState

# The daemon IPC uses AF_UNIX sockets, which don't exist on Windows.
# The daemon itself is macOS/Linux only; skip at file scope rather
# than littering each test.
pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="AF_UNIX sockets are macOS/Linux only",
)


@pytest.fixture
def short_socket_path():
    # macOS limits AF_UNIX paths to ~104 chars; pytest's tmp_path
    # nests deep enough to blow that. Use /tmp-rooted tempdir instead.
    with tempfile.TemporaryDirectory(prefix="sy-ipc-") as d:
        yield Path(d) / "daemon.sock"


def _dummy_state() -> IPCState:
    return IPCState(
        tunnel_backend="tailscale",
        tunnel_url="https://example.ts.net",
        tunnel_verified_at=None,
        subscribers=0,
        last_event_at=None,
        registered_repos=["org/repo"],
        rate_limit=None,
    )


def _read_lines_blocking(sock: socket.socket, count: int, timeout: float = 3.0) -> list[dict]:
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


def test_subscribe_then_receive_broadcast(short_socket_path: Path) -> None:
    async def run() -> None:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
        )
        await server.start()
        try:
            def client_fn() -> list[dict]:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(str(short_socket_path))
                try:
                    sock.sendall(b'{"type":"subscribe"}\n')
                    return _read_lines_blocking(sock, count=2)
                finally:
                    sock.close()

            lines_task = asyncio.create_task(asyncio.to_thread(client_fn))

            await asyncio.sleep(0.2)
            await server.broadcast_event({"kind": "workflow_run", "payload": {"x": 1}})
            lines = await asyncio.wait_for(lines_task, timeout=3.0)
        finally:
            await server.stop()

        assert lines[0]["type"] == "hello"
        assert lines[1]["type"] == "event"
        assert lines[1]["kind"] == "workflow_run"
        assert lines[1]["payload"] == {"x": 1}

    asyncio.run(run())


def test_late_subscriber_gets_ring_buffer_backlog(short_socket_path: Path) -> None:
    async def run() -> None:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
        )
        await server.start()
        try:
            await server.broadcast_event({"kind": "workflow_run", "payload": {"id": 1}})
            await server.broadcast_event({"kind": "workflow_run", "payload": {"id": 2}})

            def client_fn() -> list[dict]:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(str(short_socket_path))
                try:
                    sock.sendall(b'{"type":"subscribe"}\n')
                    return _read_lines_blocking(sock, count=3)
                finally:
                    sock.close()

            lines = await asyncio.wait_for(asyncio.to_thread(client_fn), timeout=3.0)
        finally:
            await server.stop()

        assert lines[0]["type"] == "hello"
        assert [(x["type"], x["payload"]["id"]) for x in lines[1:]] == [
            ("event", 1),
            ("event", 2),
        ]

    asyncio.run(run())


def test_status_request_returns_snapshot(short_socket_path: Path) -> None:
    async def run() -> None:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
        )
        await server.start()
        try:
            def client_fn() -> list[dict]:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(str(short_socket_path))
                try:
                    sock.sendall(b'{"type":"status"}\n')
                    return _read_lines_blocking(sock, count=2)
                finally:
                    sock.close()

            lines = await asyncio.wait_for(asyncio.to_thread(client_fn), timeout=3.0)
        finally:
            await server.stop()

        assert lines[0]["type"] == "hello"
        assert lines[1]["type"] == "status"
        assert lines[1]["tunnel"]["backend"] == "tailscale"
        assert lines[1]["registered_repos"] == ["org/repo"]

    asyncio.run(run())
