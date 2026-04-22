"""Transport layer for ``shipyard wait``.

Encapsulates the contract defined in ``docs/waiting.md``:

1. Open IPC subscription first. Start buffering incoming events.
2. Take one authoritative ``gh`` snapshot. If it already matches,
   exit 0 immediately — regardless of daemon state or ``--no-fallback``.
3. Not matched + daemon available → drain buffer, then consume live
   events, re-evaluating on every event.
4. Not matched + daemon unavailable + fallback allowed → poll ``gh``.
5. Not matched + daemon unavailable + ``--no-fallback`` → exit 6.

Return values carry telemetry (transport mode, event count, snapshot vs
live hit) so the CLI's ``--json`` surface can report faithfully.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import socket
import subprocess
import time
from collections.abc import Callable
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from shipyard.ship.wait import (
    InvalidInputError,
    RunFailedFastError,
    TruthResult,
    UnsupportedScopeError,
)

# Evaluator: given the latest snapshot dict from ``gh``, return a
# TruthResult. Pure — all I/O lives in this module.
Evaluator = Callable[[dict[str, Any] | None], TruthResult]

# FetchSnapshot: do the authoritative gh call and return the parsed
# snapshot (or None if the resource doesn't exist). May raise
# subprocess.CalledProcessError on transport failure.
FetchSnapshot = Callable[[], "dict[str, Any] | None"]

# EventFilter: given a decoded IPC ``event`` payload, return True if the
# event is relevant to this waiter (same PR, same repo, etc.). Irrelevant
# events are discarded without triggering a re-snapshot.
EventFilter = Callable[[dict[str, Any]], bool]


@dataclass
class WaitOutcome:
    matched: bool = False
    observed: dict[str, Any] = field(default_factory=dict)
    transport: str = "polling"  # "daemon" | "polling"
    fallback_used: bool = False
    events_received: int = 0
    timed_out: bool = False
    daemon_unavailable: bool = False
    fallback_disabled_hit: bool = False
    elapsed_seconds: float = 0.0


def _default_socket_path() -> Path:
    # Avoid importing Config from the CLI just for the state_dir.
    import sys as _sys

    if _sys.platform == "darwin":
        return Path.home() / "Library" / "Application Support" / "shipyard" / "daemon" / "daemon.sock"
    if _sys.platform == "win32":
        # Daemon is not supported on Windows, but keep parity for tests.
        return Path.home() / "AppData" / "Local" / "shipyard" / "daemon" / "daemon.sock"
    return Path.home() / ".local" / "state" / "shipyard" / "daemon" / "daemon.sock"


def wait_for_condition(
    *,
    evaluator: Evaluator,
    fetch_snapshot: FetchSnapshot,
    event_filter: EventFilter,
    timeout_seconds: float,
    poll_interval_seconds: float,
    no_fallback: bool,
    socket_path: Path | None = None,
) -> WaitOutcome:
    """Run the canonical subscribe / snapshot / poll-fallback flow.

    Synchronous wrapper around the async orchestration so the CLI layer
    stays non-async. The one asyncio loop we create is short-lived and
    scoped to this call.
    """
    return asyncio.run(
        _wait_for_condition_async(
            evaluator=evaluator,
            fetch_snapshot=fetch_snapshot,
            event_filter=event_filter,
            timeout_seconds=timeout_seconds,
            poll_interval_seconds=poll_interval_seconds,
            no_fallback=no_fallback,
            socket_path=socket_path or _default_socket_path(),
        )
    )


async def _wait_for_condition_async(
    *,
    evaluator: Evaluator,
    fetch_snapshot: FetchSnapshot,
    event_filter: EventFilter,
    timeout_seconds: float,
    poll_interval_seconds: float,
    no_fallback: bool,
    socket_path: Path,
) -> WaitOutcome:
    start = time.monotonic()
    deadline = start + timeout_seconds
    outcome = WaitOutcome()

    # --- step 1: subscribe (best effort) ---------------------------
    conn = await _try_connect(socket_path)
    incoming: asyncio.Queue[dict[str, Any]] = asyncio.Queue()
    reader_task: asyncio.Task[None] | None = None
    if conn is not None:
        outcome.transport = "daemon"
        reader_task = asyncio.create_task(_reader_loop(conn, incoming))
    else:
        outcome.transport = "polling"
        outcome.daemon_unavailable = True

    try:
        # --- step 2: authoritative snapshot ------------------------
        first_snapshot = await _fetch_via_thread(fetch_snapshot)
        first_result = evaluator(first_snapshot)
        outcome.observed = first_result.observed
        outcome.matched = first_result.matched
        if first_result.matched:
            outcome.elapsed_seconds = time.monotonic() - start
            return outcome

        # --- step 6: strict-mode exit when snapshot missed -----------
        if conn is None and no_fallback:
            outcome.fallback_disabled_hit = True
            outcome.elapsed_seconds = time.monotonic() - start
            return outcome

        # --- step 4: drain + live events via daemon ----------------
        if conn is not None:
            while True:
                now = time.monotonic()
                if now >= deadline:
                    outcome.timed_out = True
                    break
                try:
                    event = await asyncio.wait_for(
                        incoming.get(), timeout=max(0.01, deadline - now)
                    )
                except TimeoutError:
                    outcome.timed_out = True
                    break
                if event.get("_disconnect"):
                    # Daemon went away mid-wait. Fall back to polling
                    # (or exit 6) per the contract.
                    outcome.daemon_unavailable = True
                    break
                if not event_filter(event):
                    continue
                outcome.events_received += 1
                snapshot = await _fetch_via_thread(fetch_snapshot)
                result = evaluator(snapshot)
                outcome.observed = result.observed
                if result.matched:
                    outcome.matched = True
                    outcome.elapsed_seconds = time.monotonic() - start
                    return outcome
            if outcome.timed_out:
                outcome.elapsed_seconds = time.monotonic() - start
                return outcome
            # If we're here the subscription died. If fallback is
            # disabled, exit strict.
            if no_fallback:
                outcome.fallback_disabled_hit = True
                outcome.elapsed_seconds = time.monotonic() - start
                return outcome

        # --- step 5: polling fallback ------------------------------
        outcome.transport = "polling"
        outcome.fallback_used = conn is not None
        while True:
            now = time.monotonic()
            if now >= deadline:
                outcome.timed_out = True
                break
            sleep_for = min(poll_interval_seconds, deadline - now)
            await asyncio.sleep(sleep_for)
            snapshot = await _fetch_via_thread(fetch_snapshot)
            result = evaluator(snapshot)
            outcome.observed = result.observed
            if result.matched:
                outcome.matched = True
                outcome.elapsed_seconds = time.monotonic() - start
                return outcome

    finally:
        if reader_task is not None and not reader_task.done():
            reader_task.cancel()
            with contextlib.suppress(asyncio.CancelledError):
                await reader_task
        if conn is not None:
            await _close_connection(conn)

    outcome.elapsed_seconds = time.monotonic() - start
    return outcome


async def _fetch_via_thread(fetch: FetchSnapshot) -> dict[str, Any] | None:
    """Run a sync ``gh`` call in a thread so we don't block the loop."""
    return await asyncio.to_thread(fetch)


# ---------------------------------------------------------------------------
# IPC plumbing (async)


@dataclass
class _Connection:
    reader: asyncio.StreamReader
    writer: asyncio.StreamWriter


async def _try_connect(socket_path: Path) -> _Connection | None:
    """Open the daemon socket + send a subscribe frame. Returns None if
    the daemon is unreachable for any reason."""
    if not socket_path.exists():
        return None
    try:
        reader, writer = await asyncio.open_unix_connection(path=str(socket_path))
    except (ConnectionError, FileNotFoundError, PermissionError, OSError):
        return None
    try:
        writer.write(b'{"type":"subscribe"}\n')
        await writer.drain()
    except (ConnectionError, OSError):
        with contextlib.suppress(Exception):
            writer.close()
        return None
    return _Connection(reader=reader, writer=writer)


async def _close_connection(conn: _Connection) -> None:
    with contextlib.suppress(ConnectionError, OSError):
        conn.writer.close()
        with contextlib.suppress(TimeoutError, ConnectionError, OSError):
            await asyncio.wait_for(conn.writer.wait_closed(), timeout=1.0)


async def _reader_loop(
    conn: _Connection, queue: asyncio.Queue[dict[str, Any]]
) -> None:
    """Forward every decoded frame onto the queue.

    Ring-buffer replays and live events arrive as ``{"type":"event",
    "kind": "...", "payload": {...}}`` frames — identical shape. The
    waiter treats them the same per v4.
    """
    try:
        while True:
            line = await conn.reader.readline()
            if not line:
                await queue.put({"_disconnect": True})
                return
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if not isinstance(msg, dict):
                continue
            msg_type = msg.get("type")
            if msg_type == "event":
                await queue.put(msg)
            elif msg_type == "goodbye":
                await queue.put({"_disconnect": True})
                return
            # hello / status frames: ignored by the waiter
    except (ConnectionError, OSError):
        await queue.put({"_disconnect": True})


# ---------------------------------------------------------------------------
# Blocking one-shot helpers for callers that just want "daemon up?"


def daemon_available(socket_path: Path | None = None) -> bool:
    """Best-effort synchronous probe, used by CLI preflight."""
    path = socket_path or _default_socket_path()
    if not path.exists():
        return False
    try:
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        sock.settimeout(0.5)
        sock.connect(str(path))
        sock.close()
        return True
    except OSError:
        return False


# ---------------------------------------------------------------------------
# gh snapshot helpers (thin wrappers used by the CLI)


def _gh(args: list[str], timeout_seconds: float = 20.0) -> tuple[int, str]:
    """Run a ``gh`` command, returning (returncode, stdout)."""
    try:
        res = subprocess.run(
            ["gh", *args],
            capture_output=True,
            text=True,
            timeout=timeout_seconds,
            check=False,
        )
    except (subprocess.TimeoutExpired, FileNotFoundError) as exc:
        raise _GhError(str(exc)) from exc
    return res.returncode, (res.stdout or "")


class _GhError(Exception):
    pass


def fetch_release_snapshot(*, repo: str, tag: str) -> dict[str, Any] | None:
    """Look up the GitHub release at ``tag``. Returns ``None`` if not
    found, raises :class:`InvalidInputError` for malformed tags."""
    if not tag:
        raise InvalidInputError("empty tag")
    code, out = _gh(
        [
            "api",
            f"repos/{repo}/releases/tags/{tag}",
            "-H",
            "Accept: application/vnd.github+json",
        ],
        timeout_seconds=15.0,
    )
    if code != 0:
        # `gh api` writes "404" / "Not Found" to stdout on error when
        # --method/-H combined with a missing route; fall back to None
        # rather than surfacing the raw stderr.
        return None
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    return data


def fetch_pr_snapshot(
    *, repo: str, pr_number: int
) -> dict[str, Any] | None:
    code, out = _gh(
        [
            "pr",
            "view",
            str(pr_number),
            "--repo",
            repo,
            "--json",
            "number,headRefOid,state,merged,mergeable,mergeStateStatus,statusCheckRollup",
        ],
        timeout_seconds=20.0,
    )
    if code != 0:
        return None
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    return data


def fetch_run_snapshot(
    *, repo: str, run_id: str
) -> dict[str, Any] | None:
    code, out = _gh(
        [
            "run",
            "view",
            run_id,
            "--repo",
            repo,
            "--json",
            "databaseId,status,conclusion,headSha,workflowName,url",
        ],
        timeout_seconds=15.0,
    )
    if code != 0:
        return None
    try:
        data = json.loads(out)
    except json.JSONDecodeError:
        return None
    if not isinstance(data, dict):
        return None
    return data


# ---------------------------------------------------------------------------
# Event-filter factories


def pr_event_filter(pr_number: int, repo: str) -> EventFilter:
    """Only forward events that clearly concern this PR.

    Deliberately loose: we re-fetch the authoritative snapshot on any
    match, so false positives just cost a ``gh`` call. False negatives
    would cause missed wakes, which is what the poll-fallback is for.
    """

    def _filter(event: dict[str, Any]) -> bool:
        kind = event.get("kind")
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
            return False
        if kind in {"pull_request"}:
            return payload.get("number") == pr_number
        if kind in {"check_run", "check_suite"}:
            nums = payload.get("pull_request_numbers") or []
            return pr_number in nums or payload.get("repo") == repo
        if kind == "workflow_run":
            return payload.get("repo") == repo
        if kind == "reconcile_healed":
            return payload.get("pr") == pr_number and payload.get("repo") == repo
        return False

    return _filter


def run_event_filter(run_id: str, repo: str) -> EventFilter:
    def _filter(event: dict[str, Any]) -> bool:
        kind = event.get("kind")
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
            return False
        if kind == "workflow_run":
            return (
                str(payload.get("run_id")) == str(run_id)
                and payload.get("repo") == repo
            )
        if kind == "workflow_job":
            return str(payload.get("run_id")) == str(run_id)
        return False

    return _filter


def release_event_filter(tag: str, repo: str) -> EventFilter:
    def _filter(event: dict[str, Any]) -> bool:
        kind = event.get("kind")
        payload = event.get("payload") or {}
        if not isinstance(payload, dict):
            return False
        if kind == "release":
            return (
                payload.get("tag_name") == tag
                and payload.get("repo") == repo
            )
        return False

    return _filter


__all__ = [
    "WaitOutcome",
    "wait_for_condition",
    "daemon_available",
    "fetch_release_snapshot",
    "fetch_pr_snapshot",
    "fetch_run_snapshot",
    "pr_event_filter",
    "run_event_filter",
    "release_event_filter",
    "InvalidInputError",
    "UnsupportedScopeError",
    "RunFailedFastError",
    "TruthResult",
]
