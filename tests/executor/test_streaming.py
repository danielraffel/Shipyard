from __future__ import annotations

import sys

from shipyard.executor.streaming import run_streaming_command


def test_run_streaming_command_parses_phase_markers(tmp_path) -> None:
    seen: list[dict] = []
    log_path = tmp_path / "stream.log"

    result = run_streaming_command(
        [
            sys.executable,
            "-c",
            "print('__SHIPYARD_PHASE__:build'); print('building')",
        ],
        log_path=str(log_path),
        progress_callback=lambda fields: seen.append(dict(fields)),
    )

    assert result.returncode == 0
    assert result.phase == "build"
    assert "building" in result.output
    assert any(item.get("phase") == "build" for item in seen)
    assert "building" in log_path.read_text()


def test_run_streaming_command_emits_stuck_heartbeat(tmp_path) -> None:
    # #186 was a timing flake on macOS ARM CI under load: the subprocess
    # completed before any heartbeat thread tick fired in the "stuck"
    # window. Widened the budget: 0.8s quiet subprocess, 0.05s heartbeat
    # cadence, 0.15s stuck threshold — leaves ~0.65s where the scheduler
    # has ample chances to land a ``liveness=stuck`` emission even under
    # a noisy runner. Trade 0.6s extra wall-clock time for determinism.
    seen: list[dict] = []

    result = run_streaming_command(
        [
            sys.executable,
            "-c",
            "import time; time.sleep(0.8); print('done')",
        ],
        log_path=str(tmp_path / "heartbeat.log"),
        heartbeat_interval_secs=0.05,
        stuck_idle_secs=0.15,
        progress_callback=lambda fields: seen.append(dict(fields)),
    )

    assert result.returncode == 0
    heartbeats = [item for item in seen if item.get("last_heartbeat_at")]
    assert heartbeats
    assert any(item.get("liveness") == "stuck" for item in heartbeats)
