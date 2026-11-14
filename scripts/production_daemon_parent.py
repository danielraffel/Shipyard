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


def classify_parent(
    daemon_pid: int,
    *,
    launcher: str,
    installed: str,
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
    reader: ProcessReader = read_process,
    clock: Callable[[], float] = time.monotonic,
    sleep: Callable[[float], None] = time.sleep,
) -> tuple[bool, str]:
    deadline = clock() + wait_seconds
    while True:
        ok, description = classify_parent(
            daemon_pid, launcher=launcher, installed=installed, reader=reader
        )
        if ok or clock() >= deadline:
            return ok, description
        sleep(1)


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--installed", required=True)
    parser.add_argument("--launcher", default=str(Path.home() / LAUNCHER_RELATIVE_PATH))
    parser.add_argument("--wait-seconds", type=float, default=10.0)
    args = parser.parse_args(argv)
    ok, description = wait_for_supervised_parent(
        args.pid,
        launcher=args.launcher,
        installed=args.installed,
        wait_seconds=args.wait_seconds,
    )
    print(f"production daemon {args.pid}: {description}", file=sys.stderr if not ok else sys.stdout)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
