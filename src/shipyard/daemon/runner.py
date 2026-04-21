"""Process-lifecycle glue for ``shipyard daemon``.

Split out from ``controller.py`` so the CLI layer doesn't need to
reach into asyncio internals. Callers get three verbs:

    * ``run_blocking()`` — foreground daemon, blocks until signalled.
    * ``spawn_detached()`` — background daemon, fire-and-forget.
    * ``stop_running()`` — ask a running daemon to exit cleanly.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import logging
import os
import socket
import subprocess
import sys
import time
from typing import TYPE_CHECKING

from shipyard.daemon import disclosure
from shipyard.daemon.controller import Daemon, DaemonAlreadyRunningError, DaemonConfig
from shipyard.daemon.tunnels.base import TunnelNotReadyError, TunnelStartError

if TYPE_CHECKING:
    from pathlib import Path

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


def _spawn_argv() -> list[str]:
    """Choose the argv prefix for spawning the daemon subprocess.

    Two install shapes to handle:

    * **pip / uv install** — ``sys.executable`` is a Python interpreter
      (basename ``python`` / ``python3`` / ``pypy3``). Shipyard is
      imported as a module; we invoke it via ``-m shipyard``.
    * **Standalone binary** — shiv zipapp, PyInstaller bundle, etc.
      (e.g. the ``install.sh`` drop-in at ``~/.local/bin/shipyard``).
      ``sys.executable`` is the shipyard binary itself; passing ``-m``
      makes Click error with ``No such option: -m`` and the daemon
      never starts. Invoke the binary directly instead.

    Detection is by basename — Python interpreters always contain
    "python" (or "pypy"); the shipyard binary never does.
    """
    exe_name = os.path.basename(sys.executable).lower()
    if "python" in exe_name or "pypy" in exe_name:
        return [sys.executable, "-m", "shipyard", "daemon", "run"]
    # Anything else is the shipyard binary itself.
    return [sys.executable, "daemon", "run"]


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
    args = _spawn_argv()
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
    # Then verify the child is *still alive* — a PyInstaller-bundled
    # standalone binary can crash 100ms into startup with a
    # LookupError and write its PID before exiting, leaving callers
    # with a stale PID that no longer maps to a process. Fixed in
    # 0.22.4 after the encodings.idna bundling bug surfaced.
    deadline = time.time() + 3.0
    pid_from_file: int | None = None
    while time.time() < deadline:
        if pid_file.exists():
            try:
                pid_from_file = int(pid_file.read_text(encoding="utf-8").strip())
            except (OSError, ValueError):
                break
            break
        time.sleep(0.05)
    final_pid = pid_from_file if pid_from_file is not None else proc.pid
    # Settle window: give the child a bit to die if it's going to.
    time.sleep(0.3)
    if not _pid_alive(final_pid):
        # Harvest the tail of the log so the caller can surface a
        # useful error instead of a lie ("daemon started").
        tail = _read_log_tail(log_path)
        raise DaemonSpawnFailedError(
            f"daemon exited immediately after spawn (pid {final_pid}). "
            f"Tail of {log_path}:\n{tail}"
        )
    return final_pid


class DaemonSpawnFailedError(RuntimeError):
    """Raised when spawn_detached starts a daemon that dies before
    the verification window expires. Callers should surface the
    log tail in the exception message rather than claim success."""


def _read_log_tail(log_path: Path, *, bytes_to_read: int = 2048) -> str:
    """Return the last few KB of the daemon log as text. Best-effort —
    returns an empty string if the file doesn't exist or can't be read.
    """
    try:
        with open(log_path, "rb") as handle:
            handle.seek(0, os.SEEK_END)
            size = handle.tell()
            handle.seek(max(0, size - bytes_to_read))
            data = handle.read()
        return data.decode("utf-8", errors="replace")
    except OSError:
        return ""


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
        except (TimeoutError, OSError):
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
            with contextlib.suppress(OSError):
                os.kill(pid, 9)  # SIGKILL
            return True
        # Stale PID file — clean up.
        with contextlib.suppress(OSError):
            pid_file.unlink()
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
    timeout: float = 5.0,
) -> _EventIterator | None:
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
    except (TimeoutError, OSError):
        return None
    return _EventIterator(client)


class _EventIterator:
    """Line-buffered iterator over NDJSON messages from the daemon."""

    def __init__(self, client: socket.socket) -> None:
        self._client = client
        self._client.settimeout(None)
        self._buf = b""

    def __iter__(self) -> _EventIterator:
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
        with contextlib.suppress(OSError):
            self._client.close()
