#!/usr/bin/env python3
"""Stop starting repair model passes on a rescue entry that has not changed.

A rescue-queue line is ``<repo>#<n> <head sha> <REASON>``, where the reason
names what blocks the head (``HOLD``, ``FAILED:<checks>``, ``CONFLICT``, ...).
Every input a repair pass acts on is in that line: a push changes the sha, a
rerun that turns a check green or red changes the ``FAILED:`` set, a merged
stack base changes the reason. When the identical line has already been
attempted ``--max`` times inside ``--window`` seconds, another pass sees the
same facts the earlier ones did, so the runner does not start one: the entry
is held and named, and it returns by itself as soon as its line changes or
the window passes.

Why: in the 600 repair passes logged up to 2026-11-11, 408 were the fourth or
later attempt on the same head with nothing pushed in between (one Pulp head
drew 146). The tier ladder already escalates those to the strongest model,
which made each repeat more expensive, not more likely to finish.

    repair_budget.py filter --log FILE [--held-file FILE] [--max 3] [--window 21600] [--now EPOCH] < list > runnable
    repair_budget.py record --log FILE --line LINE [--now EPOCH]

``filter`` copies runnable lines to stdout in order and writes one
``HELD <line> attempts=<k>`` line per held entry to stderr. ``record`` appends
``<epoch>\t<line>``; the log keeps its newest 2000 lines. An unreadable log
holds nothing (it can only cost a pass, never hide an entry).
"""
from __future__ import annotations

import argparse
import os
import sys
import time

KEEP = 2000


def _norm(line: str) -> str:
    return " ".join(line.split())


def attempts(log: str, now: float, window: float) -> dict[str, int]:
    counts: dict[str, int] = {}
    try:
        with open(log) as f:
            for raw in f:
                ts, _, line = raw.rstrip("\n").partition("\t")
                try:
                    t = float(ts)
                except ValueError:
                    continue
                if 0 <= now - t <= window and line:
                    k = _norm(line)
                    counts[k] = counts.get(k, 0) + 1
    except OSError:
        return {}
    return counts


def runnable(lines: list[str], log: str, now: float, max_attempts: int, window: float):
    counts = attempts(log, now, window)
    run, held = [], []
    for line in lines:
        line = line.rstrip("\n")
        if not line.strip():
            continue
        k = counts.get(_norm(line), 0)
        (held if k >= max_attempts else run).append((line, k))
    return [l for l, _ in run], held


def record(log: str, line: str, now: float) -> None:
    try:
        with open(log) as f:
            old = f.readlines()[-(KEEP - 1):]
    except OSError:
        old = []
    tmp = f"{log}.{os.getpid()}.tmp"
    with open(tmp, "w") as f:
        f.writelines(old)
        f.write(f"{int(now)}\t{_norm(line)}\n")
    os.replace(tmp, log)


def write_held(path: str, held: list[tuple[str, int]], now: float) -> None:
    """Publish the current held entries for the scheduled operator feed."""
    tmp = f"{path}.{os.getpid()}.tmp"
    with open(tmp, "w") as f:
        for line, k in held:
            f.write(f"{int(now)}\tHELD {line} attempts={k}\n")
    os.replace(tmp, path)


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    fp = sub.add_parser("filter")
    fp.add_argument("--log", required=True)
    fp.add_argument("--held-file")
    fp.add_argument("--max", type=int, default=3)
    fp.add_argument("--window", type=float, default=6 * 3600)
    fp.add_argument("--now", type=float)
    rp = sub.add_parser("record")
    rp.add_argument("--log", required=True)
    rp.add_argument("--line", required=True)
    rp.add_argument("--now", type=float)
    a = ap.parse_args(argv)
    now = a.now if a.now is not None else time.time()
    if a.cmd == "filter":
        if a.max < 1:
            ap.error("--max must be at least 1")
        run, held = runnable(sys.stdin.readlines(), a.log, now, a.max, a.window)
        for line in run:
            print(line)
        if a.held_file:
            write_held(a.held_file, held, now)
        for line, k in held:
            print(f"HELD {line} attempts={k}", file=sys.stderr)
        return 0
    if not a.line.strip():
        ap.error("--line is empty")
    record(a.log, a.line, now)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
