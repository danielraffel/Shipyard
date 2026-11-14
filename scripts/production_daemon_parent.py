#!/usr/bin/env python3
"""Prove the production daemon is still owned by launchd's supervision chain.

After a sandbox canary on a production host, the production daemon must not
have been replaced, killed, or reparented under anything the sandbox started.
Two parent shapes are legitimate:

* ``launchd``: the daemon was spawned directly and is parented by pid 1.
* the daemon launcher: on hosts with ``shipyard daemon launcher install``, a
  per-user LaunchAgent runs the stable copy at
  ``~/.local/libexec/shipyard/shipyard-daemon-launcher`` as
  ``--mode shipyard daemon supervise --exec <installed> ...``. It stays
  resident as the daemon's parent (see ``src/daemon_launcher.rs``), and it must
  itself be parented by pid 1.

* ShipyardLauncher.app: on hosts that activated it
  (``tools/shipyard-launcher``), the daemon agent's program is the bundle
  executable with the same arguments. It spawns the current release (the
  file ``~/.local/bin/shipyard`` resolves to, normally
  ``~/.local/share/shipyard/auth-generations/<id>/shipyard``) as
  ``daemon supervise``, which is the daemon's parent. The bundle must itself
  be parented by pid 1.

Anything else, including a launcher whose own parent is not launchd or an
``--in-place`` hand-off process, fails.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from pathlib import Path
from typing import Callable, NamedTuple, Optional


LAUNCHD_PID = 1
LAUNCHER_RELATIVE_PATH = Path(".local/libexec/shipyard/shipyard-daemon-launcher")
APP_RELATIVE_PATH = Path(".local/libexec/ShipyardLauncher.app/Contents/MacOS/shipyard-launcher")
GENERATIONS_RELATIVE_PATH = Path(".local/share/shipyard/auth-generations")


class ProcessInfo(NamedTuple):
    ppid: int
    command: str


ProcessReader = Callable[[int], Optional[ProcessInfo]]


def read_process(pid: int) -> Optional[ProcessInfo]:
    """Return the parent pid and full command of ``pid``, or None if gone."""

    result = subprocess.run(
        ["ps", "-p", str(pid), "-o", "ppid=", "-o", "command="],
        capture_output=True,
        text=True,
        check=False,
    )
    line = result.stdout.strip()
    if result.returncode != 0 or not line:
        return None
    parts = line.split(None, 1)
    ppid_text, command = parts[0], parts[1] if len(parts) > 1 else ""
    try:
        ppid = int(ppid_text)
    except ValueError:
        return None
    return ProcessInfo(ppid=ppid, command=command.strip())


def is_launcher_command(command: str, *, launcher: str, installed: str) -> bool:
    """Match the exact argv shape ``launcher_arguments`` gives launchd."""

    prefix = f"{launcher} --mode shipyard "
    if not command.startswith(prefix):
        return False
    tokens = command[len(prefix):].split(" ")
    if "--in-place" in tokens:
        return False
    marker = ["daemon", "supervise", "--exec", installed]
    for index in range(len(tokens) - len(marker) + 1):
        if tokens[index:index + len(marker)] == marker:
            remainder = tokens[index + len(marker):]
            # Only `--repo <slug>` pairs may follow the exec target.
            if len(remainder) % 2 != 0:
                return False
            return all(flag == "--repo" for flag in remainder[0::2])
    return False


def release_supervisor(command: str, *, installed: str, generations: str) -> Optional[str]:
    """The release path a launcher-app child runs ``daemon supervise`` from:
    ``installed`` itself or ``<generations>/<64 hex>/shipyard``."""

    candidates = [installed]
    head = command.split(" ", 1)[0]
    prefix = generations.rstrip("/") + "/"
    if head.startswith(prefix):
        parts = head[len(prefix):].split("/")
        if (
            len(parts) == 2
            and parts[1] == "shipyard"
            and len(parts[0]) == 64
            and all(c in "0123456789abcdef" for c in parts[0])
        ):
            candidates.append(head)
    for candidate in candidates:
        if is_launcher_command(command, launcher=candidate, installed=installed):
            return candidate
    return None


def classify_parent(
    daemon_pid: int,
    *,
    launcher: str,
    installed: str,
    app: str = "",
    generations: str = "",
    reader: ProcessReader = read_process,
) -> tuple[bool, str]:
    """Return ``(ok, description)`` for the daemon's current parent chain."""

    daemon = reader(daemon_pid)
    if daemon is None:
        return False, f"production daemon pid {daemon_pid} is not running"
    if daemon.ppid == LAUNCHD_PID:
        return True, "parent is launchd"
    parent = reader(daemon.ppid)
    if parent is None:
        return False, f"parent pid {daemon.ppid} is not running"
    if app and generations and release_supervisor(
        parent.command, installed=installed, generations=generations
    ):
        bundle = reader(parent.ppid)
        if bundle is None:
            return False, f"launcher app pid {parent.ppid} is not running"
        if not is_launcher_command(bundle.command, launcher=app, installed=installed):
            return False, f"parent pid {daemon.ppid} is not run by ShipyardLauncher.app: {bundle.command}"
        if bundle.ppid != LAUNCHD_PID:
            return False, f"launcher app pid {parent.ppid} is parented by {bundle.ppid}, not launchd"
        return True, (
            f"parent is the release supervisor pid {daemon.ppid} under ShipyardLauncher.app "
            f"pid {parent.ppid} under launchd"
        )
    if not is_launcher_command(parent.command, launcher=launcher, installed=installed):
        return False, f"parent pid {daemon.ppid} is not the daemon launcher: {parent.command}"
    if parent.ppid != LAUNCHD_PID:
        return False, (
            f"daemon launcher pid {daemon.ppid} is parented by {parent.ppid}, not launchd"
        )
    return True, f"parent is the daemon launcher pid {daemon.ppid} under launchd"


def wait_for_supervised_parent(
    daemon_pid: int,
    *,
    launcher: str,
    installed: str,
    wait_seconds: float,
    app: str = "",
    generations: str = "",
    reader: ProcessReader = read_process,
    clock: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
) -> tuple[bool, str]:
    deadline = clock() + wait_seconds
    while True:
        ok, description = classify_parent(
            daemon_pid,
            launcher=launcher,
            installed=installed,
            app=app,
            generations=generations,
            reader=reader,
        )
        if ok or clock() >= deadline:
            return ok, description
        sleep(1)


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--installed", required=True)
    parser.add_argument("--launcher", default=str(Path.home() / LAUNCHER_RELATIVE_PATH))
    parser.add_argument("--app", default=str(Path.home() / APP_RELATIVE_PATH))
    parser.add_argument("--generations", default=str(Path.home() / GENERATIONS_RELATIVE_PATH))
    parser.add_argument("--wait-seconds", type=float, default=10.0)
    args = parser.parse_args(argv)
    ok, description = wait_for_supervised_parent(
        args.pid,
        launcher=args.launcher,
        installed=args.installed,
        app=args.app,
        generations=args.generations,
        wait_seconds=args.wait_seconds,
    )
    print(f"production daemon {args.pid}: {description}", file=sys.stderr if not ok else sys.stdout)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
