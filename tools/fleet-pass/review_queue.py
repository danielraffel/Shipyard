#!/usr/bin/env python3
"""Deterministic filters for the reviewer work list, so no model pass is spent
on a head that needs no new review.

Two rules, both decided from facts the scheduler already holds:

* ``gate_carried(statuses, head, approved_heads, now)``: the head's newest
  ``review-verdict`` commit status says the gate CARRIED an earlier approval to
  this exact head, and that approval is one the queue already counts. The gate
  (``review_verdict_status.py`` over ``ghapp_queue_arm_guard.review_gate``)
  posts ``success`` for every allowed decision, including "this repository does
  not require a review", so ``success`` alone is not enough: the description
  must read ``approved: the approval of <12 hex> carries to <head[:12]>`` (the
  merge-only carry) or ``... carries across the rebase to <head[:12]>`` (the
  rebase carry), the approved prefix must match a counted APPROVE in the PR's
  comments, and the status must be younger than ``CARRY_STATUS_MAX_AGE`` (the
  guard stops honouring a carry after 48 h and the status is not recomputed).
  Neither carry applies when the base also changed a file the pull request
  changes, so those heads stay queued. Skipping changes nothing about what is
  admitted: the steward and the required check decide from their own guard.
  In the week to 2026-11-11, 17 re-reviews landed on merge-only heads whose
  files all kept their approved blobs and none was a HOLD; the five merge-only
  re-reviews that did HOLD all followed a base change to a file the pull
  request changes.

* ``claim``: give each reviewer slot only the entries it holds an exclusive
  claim on. Listing the unclaimed rest after the slot's first claim let two
  slots review the same head within seconds (Shipyard #1216 and #1201 drew
  two APPROVEs 26 s and 50 s apart). In the week to 2026-11-11, 152 of 1,185
  verdicts repeated the same signer on a head it had already judged, 138 of
  them within ten minutes of the first.

``claim`` is the CLI the runner calls:

    review_queue.py claim --dir CLAIMS --pid PID --cap N < list > mine

Each input line is ``<repo>#<n> <sha> ...``. A claim is a directory
``CLAIMS/<repo>_<n>`` holding ``pid``; it is live while that pid runs, and a
dead pid's claim is taken over. At most ``N`` lines are claimed and printed,
in input order. Nothing else is printed, so a slot never sees a head another
live slot holds.
"""
from __future__ import annotations

import argparse
import datetime
import os
import re
import shutil
import sys
import tempfile
import time

GATE = "review-verdict"


CARRY_STATUS_MAX_AGE = 24 * 3600
_CARRY = re.compile(r"^approved: the approval of ([0-9a-f]{12}) carries (?:to|across the rebase to) ([0-9a-f]{12})\b")


def _ts(s: str) -> float | None:
    try:
        return datetime.datetime.fromisoformat(s.replace("Z", "+00:00")).timestamp()
    except (AttributeError, ValueError):
        return None


def gate_carried(statuses: list, head: str, approved_heads, now: float) -> bool:
    """True only when the newest ``review-verdict`` status names a carry to ``head``
    from an approval in ``approved_heads`` (full shas of counted APPROVEs), and is
    younger than ``CARRY_STATUS_MAX_AGE``. Anything else keeps the head queued."""
    gate = [s for s in statuses if s.get("context") == GATE]
    if not gate:
        return False
    newest = max(gate, key=lambda s: (s.get("created_at") or "", s.get("id") or 0))
    if newest.get("state") != "success":
        return False
    m = _CARRY.match(newest.get("description") or "")
    if not m or m.group(2) != head.lower()[:12]:
        return False
    if not any(a.lower().startswith(m.group(1)) and a.lower() != head.lower() for a in approved_heads):
        return False
    at = _ts(newest.get("created_at") or "")
    return at is not None and 0 <= now - at <= CARRY_STATUS_MAX_AGE


def _alive(pid: str) -> bool:
    try:
        n = int(pid)
    except ValueError:
        return False
    if n <= 0:
        return False
    try:
        os.kill(n, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def _key(line: str) -> str:
    return line.split()[0].replace("/", "_").replace("#", "_")


PIDLESS_GRACE = 60


def try_claim(claims_dir: str, line: str, pid: int) -> bool:
    """Atomically claim the entry, taking over a claim whose pid is dead.

    The claim directory is prepared with its ``pid`` file under a private name
    and renamed into place, so no other slot can ever see a claim without a
    pid; ``rename`` onto a held (non-empty) claim fails. A directory without a
    pid (an older runner mid-write) counts as live for ``PIDLESS_GRACE`` seconds;
    an empty one is simply replaced by the rename."""
    d = os.path.join(claims_dir, _key(line))
    for _ in range(2):
        tmp = tempfile.mkdtemp(prefix=f".{_key(line)}.{pid}.", dir=claims_dir)
        with open(os.path.join(tmp, "pid"), "w") as f:
            f.write(f"{pid}\n")
        try:
            os.rename(tmp, d)
            return True
        except OSError:
            shutil.rmtree(tmp, ignore_errors=True)
        try:
            with open(os.path.join(d, "pid")) as f:
                holder = f.read().strip()
        except FileNotFoundError:
            try:
                young = time.time() - os.path.getmtime(d) < PIDLESS_GRACE
            except OSError:
                young = False
            if young:
                return False
            holder = ""
        except OSError:
            return False
        if holder == str(pid):
            return True
        if holder and _alive(holder):
            return False
        # Stale (dead pid, or an old pid-less claim): take it over.
        shutil.rmtree(d, ignore_errors=True)
    return False


def claim(lines: list[str], claims_dir: str, pid: int, cap: int) -> list[str]:
    os.makedirs(claims_dir, exist_ok=True)
    mine: list[str] = []
    for line in lines:
        if len(mine) >= cap:
            break
        line = line.rstrip("\n")
        if not line.strip():
            continue
        if try_claim(claims_dir, line, pid):
            mine.append(line)
    return mine


def main(argv: list[str]) -> int:
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("claim")
    c.add_argument("--dir", required=True)
    c.add_argument("--pid", type=int, required=True)
    c.add_argument("--cap", type=int, default=2)
    a = ap.parse_args(argv)
    if a.cap < 1:
        ap.error("--cap must be at least 1")
    for line in claim(sys.stdin.readlines(), a.dir, a.pid, a.cap):
        print(line)
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
