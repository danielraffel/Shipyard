"""IPC ship-state-list request: daemon-side shortcut for
`shipyard --json ship-state list`.

Subscribers (primarily the macOS GUI) use this to avoid the
PyInstaller cold-start tax on every poll. See shipyard#153.
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

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason="AF_UNIX sockets are macOS/Linux only",
)


@pytest.fixture
def short_socket_path():
    with tempfile.TemporaryDirectory(prefix="sy-ssl-") as d:
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


def test_ship_state_list_returns_provider_output(
    short_socket_path: Path,
) -> None:
    """Provider returns a list of two dict entries → client sees both."""

    entries = [
        {
            "schema_version": 1,
            "pr": 151,
            "repo": "o/r",
            "branch": "feat/x",
            "dispatched_runs": [],
            "evidence_snapshot": {},
        },
        {
            "schema_version": 1,
            "pr": 152,
            "repo": "o/r",
            "branch": "fix/y",
            "dispatched_runs": [],
            "evidence_snapshot": {},
        },
    ]

    async def run() -> None:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
            ship_state_list_provider=lambda: entries,
        )
        await server.start()
        try:

            def client_fn() -> list[dict]:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(str(short_socket_path))
                try:
                    sock.sendall(b'{"type":"ship-state-list"}\n')
                    return _read_lines_blocking(sock, count=2)
                finally:
                    sock.close()

            lines = await asyncio.wait_for(
                asyncio.to_thread(client_fn), timeout=3.0
            )
        finally:
            await server.stop()

        assert lines[0]["type"] == "hello"
        assert lines[1]["type"] == "ship-state-list"
        assert lines[1]["states"] == entries

    asyncio.run(run())


def test_ship_state_list_returns_empty_when_no_provider(
    short_socket_path: Path,
) -> None:
    """If the daemon wasn't wired with a provider (shouldn't happen in
    production but keep the fallback sane), the request is still
    answered — just with an empty list."""

    async def run() -> None:
        server = IPCServer(
            socket_path=short_socket_path,
            status_provider=_dummy_state,
            ship_state_list_provider=None,
        )
        await server.start()
        try:

            def client_fn() -> list[dict]:
                sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                sock.connect(str(short_socket_path))
                try:
                    sock.sendall(b'{"type":"ship-state-list"}\n')
                    return _read_lines_blocking(sock, count=2)
                finally:
                    sock.close()

            lines = await asyncio.wait_for(
                asyncio.to_thread(client_fn), timeout=3.0
            )
        finally:
            await server.stop()

        assert lines[0]["type"] == "hello"
        assert lines[1]["type"] == "ship-state-list"
        assert lines[1]["states"] == []

    asyncio.run(run())
