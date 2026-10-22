#!/usr/bin/env python3
"""Refuse to start a sandbox canary on a host with a wedged filesystem mount.

lsof stats every mount point. When one mount is wedged (on m3, 2026-10-20: a
stuck CoreDevice DeviceFS mount), that stat blocks in the kernel in
uninterruptible wait, so lsof never returns and the guardian would hang until
its launchd job is booted out. This check stats each mount point in a child
process, the same call lsof makes, then reads its filesystem statistics and
the first entry of its root directory, and fails naming the mount and the
call the child is stuck on. A stat of a mount point's root can be answered
from the kernel's cached attributes while the filesystem's server is stuck
(on m3 the preflight passed a stat of every mount while rg hung in readdir),
so the statvfs and directory read are what reach that server. Passing says
only that those three calls returned for every mount, not that every call on
every mount will. A process in long uninterruptible wait alone is not a wedge (macOS
keeps some system extensions there for days), so those are listed only as
context.

At most one probe child is left behind on a wedged host; nothing can end a
process blocked in the kernel.

Exit codes: 0 every mount answered; 1 a mount did not answer; 2 cannot read.
"""
from __future__ import annotations

import argparse
import subprocess
import sys
import time
from typing import List, NamedTuple, Optional, Tuple

DEFAULT_TIMEOUT_SECONDS = 20.0
LONG_WAIT_SECONDS = 120
# The calls made on each mount point, in order. "readdir" reads one entry.
CALLS = ("stat", "statvfs", "readdir")
# Prints "<call> <mount point>" before each call, then "ok", so the parent can
# name the mount and call it stopped on. `hook` is a seam for tests.
PROBE = """
import os, sys
def hook(call, path):
    pass
def readdir(path):
    with os.scandir(path) as entries:
        next(entries, None)
CALLS = {"stat": os.stat, "statvfs": os.statvfs, "readdir": readdir}
for path in sys.argv[1:]:
    for call in %r:
        print(call + " " + path, flush=True)
        hook(call, path)
        try:
            CALLS[call](path)
        except OSError:
            pass
        print("ok", flush=True)
print("done", flush=True)
""" % (CALLS,)


class Stuck(NamedTuple):
    pid: int
    stat: str
    seconds: int
    command: str


def mount_points(mount_output: str) -> List[Tuple[str, str]]:
    """``(path, line)`` for each ``mount`` line, path taken from `` on ``."""
    points = []
    for line in mount_output.splitlines():
        if " on " not in line or " (" not in line:
            continue
        path = line.split(" on ", 1)[1].rsplit(" (", 1)[0]
        points.append((path, line))
    return points


def wedged_mount(probe_output: str, finished: bool) -> Optional[Tuple[str, str]]:
    """``(mount point, call)`` the probe was in when it stopped, or None if it
    finished."""
    if finished:
        return None
    last = None
    for line in probe_output.splitlines():
        call, _, path = line.partition(" ")
        if call in CALLS and path:
            last = (path, call)
        elif line == "ok":
            last = None
    return last or ("(before the first mount)", "start")


def probe_mounts(paths: List[str], timeout: float) -> Tuple[str, bool]:
    """Run the probe; ``(output so far, finished)``. A probe that does not
    finish in ``timeout`` is left running: it cannot be killed."""
    process = subprocess.Popen(
        [sys.executable, "-I", "-c", PROBE, *paths],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
        text=True,
    )
    try:
        output, _ = process.communicate(timeout=timeout)
        return output, "done" in output.splitlines()
    except subprocess.TimeoutExpired as error:
        process.kill()
        partial = error.output or ""
        if isinstance(partial, bytes):
            partial = partial.decode(errors="replace")
        return partial, False


def elapsed_seconds(etime: str) -> int:
    """``ps -o etime`` (``[[dd-]hh:]mm:ss``) in seconds."""
    days = 0
    if "-" in etime:
        day_text, etime = etime.split("-", 1)
        days = int(day_text)
    parts = [int(part) for part in etime.split(":")]
    if not 2 <= len(parts) <= 3:
        raise ValueError(f"unrecognized etime {etime!r}")
    while len(parts) < 3:
        parts.insert(0, 0)
    hours, minutes, seconds = parts
    return ((days * 24 + hours) * 60 + minutes) * 60 + seconds


def long_waits(ps_output: str, min_seconds: int = LONG_WAIT_SECONDS) -> List[Stuck]:
    """Processes in state U for at least ``min_seconds``, oldest first."""
    found: List[Stuck] = []
    for line in ps_output.splitlines():
        fields = line.split(None, 3)
        if len(fields) < 3 or not fields[0].isdigit() or "U" not in fields[1]:
            continue
        try:
            seconds = elapsed_seconds(fields[2])
        except ValueError:
            continue
        if seconds >= min_seconds:
            found.append(Stuck(int(fields[0]), fields[1], seconds,
                               fields[3] if len(fields) > 3 else ""))
    return sorted(found, key=lambda item: -item.seconds)


def _read(argv: List[str]) -> str:
    return subprocess.run(argv, capture_output=True, text=True, timeout=20, check=True).stdout


def main(argv: Optional[List[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--timeout", type=float, default=DEFAULT_TIMEOUT_SECONDS)
    parser.add_argument("--mount-output", help="read mount output from this file (tests)")
    parser.add_argument("--ps-output", help="read ps output from this file (tests)")
    args = parser.parse_args(argv)
    try:
        mount_text = (open(args.mount_output, encoding="utf-8").read() if args.mount_output
                      else _read(["/sbin/mount"]))
    except (OSError, subprocess.SubprocessError) as error:
        print(f"wedged-mount preflight: cannot read the mount table: {error}", file=sys.stderr)
        return 2
    points = mount_points(mount_text)
    if not points:
        # An empty table means the instrument read nothing, not a healthy host.
        print("wedged-mount preflight: mount listed no filesystems", file=sys.stderr)
        return 2
    started = time.monotonic()
    output, finished = probe_mounts([path for path, _ in points], args.timeout)
    stuck_on = wedged_mount(output, finished)
    if stuck_on is None:
        print(f"wedged-mount preflight: stat, statvfs and a one-entry directory read "
              f"returned for {len(points)} mount points in {time.monotonic() - started:.1f}s; "
              "this does not prove every call on every mount will")
        return 0
    stuck_path, stuck_call = stuck_on
    line = next((text for path, text in points if path == stuck_path), stuck_path)
    print(f"::error title=wedged mount::{stuck_call} of mount point {stuck_path} did not "
          f"return in "
          f"{args.timeout:g}s ({line}). lsof reaches every mount, so the sandbox guardian "
          "could hang. Fix the host (unmount the filesystem, restart its service, or reboot) "
          "before rerunning.")
    try:
        ps_text = (open(args.ps_output, encoding="utf-8").read() if args.ps_output
                   else _read(["/bin/ps", "-axo", "pid=,stat=,etime=,command="]))
        for item in long_waits(ps_text)[:20]:
            print(f"  in uninterruptible wait: pid={item.pid} seconds={item.seconds} "
                  f"{item.command[:200]}")
    except (OSError, subprocess.SubprocessError) as error:
        print(f"  (could not list processes: {error})")
    return 1


if __name__ == "__main__":
    sys.exit(main())
