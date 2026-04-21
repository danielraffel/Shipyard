"""Process-lifecycle glue for ``shipyard daemon``.

Split out from ``controller.py`` so the CLI layer doesn't need to
reach into asyncio internals. Callers get three verbs:

    * ``run_blocking()`` — foreground daemon, blocks until signalled.
    * ``spawn_detached()`` — background daemon, fire-and-forget.
    * ``stop_running()`` — ask a running daemon to exit cleanly.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import socket
import subprocess
import sys
import time
from pathlib import Path

from shipyard.daemon import disclosure
from shipyard.daemon.controller import Daemon, DaemonAlreadyRunningError, DaemonConfig
from shipyard.daemon.tunnels.base import TunnelNotReadyError, TunnelStartError

logger = logging.getLogger(__name__)


def run_blocking(*, state_dir: Path, repos: list[str]) -> int:
    """Run the daemon in-process until SIGINT/SIGTERM. Returns the
    exit code the CLI should propagate.

    Exit codes:
      0 — graceful shutdown.
      2 — another daemon is already running (PID file lock held).
      3 — tunnel backend isn't ready (Tailscale not installed /
          signed in / Funnel not permitted on the tailnet). The
          recommended remedy is to either enable Tailscale Funnel or
          skip the daemon entirely; `shipyard watch` and the macOS
          app both work without it.
    """
    logging.basicConfig(
        level=logging.INFO,
        format="%(asctime)s %(levelname)s %(name)s: %(message)s",
    )
    # Show the first-run notice before we touch Tailscale or GitHub.
    disclosure.show_if_first_run(state_dir, repos)
    config = DaemonConfig(state_dir=state_dir, repos=repos)
    daemon = Daemon(config)
    try:
        asyncio.run(_run_async(daemon))
    except DaemonAlreadyRunningError as exc:
        logger.error("%s", exc)
        return 2
    except (TunnelNotReadyError, TunnelStartError) as exc:
        logger.error(
            "Tailscale Funnel isn't available: %s. "
            "The daemon needs a public tunnel to receive GitHub webhooks. "
            "Install Tailscale + enable Funnel on your tailnet, or skip the "
            "daemon entirely — `shipyard watch` and the macOS app both fall "
            "back to polling.",
            exc,
        )
        return 3
    except KeyboardInterrupt:
        pass
    return 0


def spawn_detached(*, state_dir: Path, repos: list[str]) -> int:
    """Launch the daemon as a detached child process, return its PID.

    Uses the current Python executable + shipyard entry point so the
    child inherits the same environment. We prefer ``shipyard daemon
    run --repo …`` rather than forking so signal handlers + stdio are
    well-defined.
    """
    pid_file = state_dir / "daemon" / "daemon.pid"
    if pid_file.exists():
        try:
            existing_pid = int(pid_file.read_text(encoding="utf-8").strip())
        except (OSError, ValueError):
            existing_pid = 0
        if existing_pid > 0 and _pid_alive(existing_pid):
            return existing_pid
    args = [sys.executable, "-m", "shipyard", "daemon", "run"]
    for repo in repos:
        args.extend(["--repo", repo])
    # Close stdio so the detached process doesn't hold the parent
    # terminal. Logs go to stderr by default; we redirect to a file
    # for post-hoc debugging.
    log_path = state_dir / "daemon" / "daemon.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log_fd = os.open(
        str(log_path), os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600
    )
    try:
        proc = subprocess.Popen(  # noqa: S603 — explicit argv, trusted
            args,
            stdout=log_fd,
            stderr=log_fd,
            stdin=subprocess.DEVNULL,
            start_new_session=True,
            close_fds=True,
        )
    finally:
        os.close(log_fd)
    # Poll briefly for the PID file so callers can report accurately.
    deadline = time.time() + 3.0
    while time.time() < deadline:
        if pid_file.exists():
            try:
                return int(pid_file.read_text(encoding="utf-8").strip())
            except (OSError, ValueError):
                break
        time.sleep(0.05)
    return proc.pid


def stop_running(state_dir: Path) -> bool:
    """Ask a running daemon to shut down via IPC; fall back to
    ``SIGTERM`` on the PID file if the socket isn't responsive.
    Returns True if we believe something was stopped."""
    sock_path = state_dir / "daemon" / "daemon.sock"
    pid_file = state_dir / "daemon" / "daemon.pid"

    if sock_path.exists():
        try:
            with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
                client.settimeout(2.0)
                client.connect(str(sock_path))
                client.sendall(b'{"type":"stop"}\n')
        except (OSError, socket.timeout):
            pass
        else:
            # Give the daemon a moment to exit, then check PID file.
            deadline = time.time() + 3.0
            while time.time() < deadline:
                if not pid_file.exists():
                    return True
                time.sleep(0.1)

    # Fall back to SIGTERM via PID file.
    if pid_file.exists():
        try:
            pid = int(pid_file.read_text(encoding="utf-8").strip())
        except (OSError, ValueError):
            return False
        if pid > 0 and _pid_alive(pid):
            try:
                os.kill(pid, 15)  # SIGTERM
            except OSError:
                return False
            deadline = time.time() + 3.0
            while time.time() < deadline:
                if not _pid_alive(pid):
                    return True
                time.sleep(0.1)
            # Escalate.
            try:
                os.kill(pid, 9)  # SIGKILL
            except OSError:
                pass
            return True
        # Stale PID file — clean up.
        try:
            pid_file.unlink()
        except OSError:
            pass
    return False


def daemon_is_running(state_dir: Path) -> bool:
    pid_file = state_dir / "daemon" / "daemon.pid"
    if not pid_file.exists():
        return False
    try:
        pid = int(pid_file.read_text(encoding="utf-8").strip())
    except (OSError, ValueError):
        return False
    return _pid_alive(pid)


async def _run_async(daemon: Daemon) -> None:
    await daemon.start()
    try:
        await daemon.run()
    finally:
        await daemon.stop()


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
    except OSError:
        return False
    return True


def subscribe(
    state_dir: Path,
    *,
    on_event: "callable | None" = None,
    timeout: float = 5.0,
) -> "object | None":
    """Connect to the daemon's IPC socket and return a blocking iter
    of events. Returns None if the daemon isn't running / reachable."""
    sock_path = state_dir / "daemon" / "daemon.sock"
    if not sock_path.exists():
        return None
    try:
        client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        client.settimeout(timeout)
        client.connect(str(sock_path))
        client.sendall(b'{"type":"subscribe"}\n')
    except (OSError, socket.timeout):
        return None
    return _EventIterator(client)


class _EventIterator:
    """Line-buffered iterator over NDJSON messages from the daemon."""

    def __init__(self, client: socket.socket) -> None:
        self._client = client
        self._client.settimeout(None)
        self._buf = b""

    def __iter__(self) -> "_EventIterator":
        return self

    def __next__(self) -> dict[str, object]:
        while b"\n" not in self._buf:
            chunk = self._client.recv(65536)
            if not chunk:
                raise StopIteration
            self._buf += chunk
        line, _, rest = self._buf.partition(b"\n")
        self._buf = rest
        try:
            return json.loads(line)
        except json.JSONDecodeError:
            return self.__next__()

    def close(self) -> None:
        try:
            self._client.close()
        except OSError:
            pass
