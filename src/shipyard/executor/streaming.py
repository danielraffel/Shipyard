"""Shared subprocess streaming helpers for validation executors."""

from __future__ import annotations

import queue
import re
import subprocess
import threading
import time
from collections.abc import Callable
from contextlib import nullcontext
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

_PHASE_MARKERS = (
    re.compile(r"^===\s*([a-zA-Z0-9_-]+)\s*===$"),
    re.compile(r"^__SHIPYARD_PHASE__:(.+)$"),
    re.compile(r"^__PULP_PHASE__:(.+)$"),
)


@dataclass(frozen=True)
class StreamingCommandResult:
    """Result of a streamed subprocess execution."""

    returncode: int
    output: str
    started_at: datetime
    completed_at: datetime
    duration_secs: float
    last_output_at: datetime | None
    phase: str | None
    # Contract markers seen during the run. A "contract marker" is a
    # specific string the validation script is required to emit at
    # least once for the run to be considered authentic. The streaming
    # layer records every marker from `required_contract_markers` that
    # appears in the output; the caller can then check whether all
    # required markers were observed and treat a missing marker as a
    # contract violation regardless of process exit code.
    #
    # Markers are matched as substrings (not regexes) against each
    # decoded output line, so the validation script can emit them
    # anywhere in a line, not just at line start.
    contract_markers_seen: tuple[str, ...] = ()


ProgressCallback = Callable[[dict[str, Any]], None]


def run_streaming_command(
    cmd: list[str] | str,
    *,
    shell: bool = False,
    cwd: str | None = None,
    log_path: str | None = None,
    append: bool = False,
    timeout: float | None = None,
    phase: str | None = None,
    heartbeat_interval_secs: float = 30.0,
    stuck_idle_secs: float = 90.0,
    progress_callback: ProgressCallback | None = None,
    required_contract_markers: tuple[str, ...] | list[str] | None = None,
) -> StreamingCommandResult:
    """Run a command while streaming output to disk and progress callbacks.

    `required_contract_markers` is an optional list of substring markers
    the streaming layer should watch for in the command's output. Each
    seen marker is recorded in the result's `contract_markers_seen`
    field. The caller decides what to do about missing markers — the
    streaming layer never fails the process based on contract.
    """
    started_at = datetime.now(timezone.utc)
    start_time = time.monotonic()
    log_file = Path(log_path) if log_path else None
    if log_file:
        log_file.parent.mkdir(parents=True, exist_ok=True)

    proc = subprocess.Popen(
        cmd,
        cwd=cwd,
        shell=shell,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )

    line_queue: queue.Queue[bytes | None] = queue.Queue()
    reader = threading.Thread(
        target=_reader_thread,
        args=(proc.stdout, line_queue),
        daemon=True,
    )
    reader.start()

    output_parts: list[str] = []
    last_output_at: datetime | None = None
    last_output_monotonic = start_time
    current_phase = phase
    # Track which contract markers have appeared. Use a list (not a
    # set) so the order of first occurrence is preserved for
    # diagnostic output.
    markers_to_watch = tuple(required_contract_markers or ())
    seen_markers: list[str] = []

    try:
        mode = "a" if append else "w"
        with open(log_file, mode, encoding="utf-8") if log_file else nullcontext() as log:
            while True:
                elapsed = time.monotonic() - start_time
                if timeout is not None and elapsed > timeout:
                    proc.kill()
                    proc.wait(timeout=5)
                    raise subprocess.TimeoutExpired(cmd, timeout)

                wait_timeout = min(heartbeat_interval_secs, 0.1)
                try:
                    chunk = line_queue.get(timeout=wait_timeout)
                except queue.Empty:
                    if proc.poll() is not None and line_queue.empty():
                        break
                    _emit_heartbeat(
                        progress_callback=progress_callback,
                        last_output_at=last_output_at,
                        last_output_monotonic=last_output_monotonic,
                        now_monotonic=time.monotonic(),
                        start_monotonic=start_time,
                        current_phase=current_phase,
                        stuck_idle_secs=stuck_idle_secs,
                    )
                    continue

                if chunk is None:
                    break

                decoded = chunk.decode("utf-8", errors="replace")
                output_parts.append(decoded)
                if log:
                    log.write(decoded)
                    log.flush()

                stripped = decoded.strip()
                marker_phase = _parse_phase_marker(stripped)
                if marker_phase:
                    current_phase = marker_phase

                # Contract marker tracking. Match as substring against
                # the decoded line so the validation script can emit
                # the marker as part of a longer status message.
                if markers_to_watch:
                    for marker in markers_to_watch:
                        if marker in decoded and marker not in seen_markers:
                            seen_markers.append(marker)

                last_output_at = datetime.now(timezone.utc)
                last_output_monotonic = time.monotonic()
                if progress_callback:
                    progress_callback(
                        {
                            "phase": current_phase,
                            "last_output_at": last_output_at,
                            "quiet_for_secs": 0.0,
                            "liveness": "active",
                        }
                    )

            returncode = proc.wait(timeout=5)

    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait(timeout=5)

    completed_at = datetime.now(timezone.utc)
    return StreamingCommandResult(
        returncode=returncode,
        output="".join(output_parts),
        started_at=started_at,
        completed_at=completed_at,
        duration_secs=time.monotonic() - start_time,
        last_output_at=last_output_at,
        phase=current_phase,
        contract_markers_seen=tuple(seen_markers),
    )


def _reader_thread(stream: Any, output_queue: queue.Queue[bytes | None]) -> None:
    try:
        if stream is None:
            return
        for line in iter(stream.readline, b""):
            output_queue.put(line)
    finally:
        if stream is not None:
            stream.close()
        output_queue.put(None)


def _parse_phase_marker(line: str) -> str | None:
    for pattern in _PHASE_MARKERS:
        match = pattern.match(line)
        if match:
            return match.group(1).strip()
    return None


def _emit_heartbeat(
    *,
    progress_callback: ProgressCallback | None,
    last_output_at: datetime | None,
    last_output_monotonic: float,
    now_monotonic: float,
    start_monotonic: float,
    current_phase: str | None,
    stuck_idle_secs: float,
) -> None:
    if progress_callback is None:
        return

    quiet_for_secs = max(
        0.0,
        now_monotonic - (last_output_monotonic if last_output_at is not None else start_monotonic),
    )

    liveness = "quiet"
    if quiet_for_secs >= stuck_idle_secs:
        liveness = "stuck"

    progress_callback(
        {
            "phase": current_phase,
            "last_output_at": last_output_at,
            "last_heartbeat_at": datetime.now(timezone.utc),
            "quiet_for_secs": quiet_for_secs,
            "liveness": liveness,
        }
    )
