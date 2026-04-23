"""Regression tests for the error-detail block under the job table.

Filed as #169: ``shipyard run/pr`` used to print a bare ``ubuntu  error
ssh`` row without the underlying backend error, forcing users to cat
the log file to see what actually broke. ``render_job`` now emits a
per-target error line whenever a non-passing target carries an
``error_message``.
"""

from __future__ import annotations

import io
from datetime import datetime, timezone

from rich.console import Console

from shipyard.core.job import (
    Job,
    TargetResult,
    TargetStatus,
    ValidationMode,
)
from shipyard.output import human


def _render(job: Job) -> str:
    buf = io.StringIO()
    saved = human.console
    try:
        human.console = Console(file=buf, force_terminal=False, width=120)
        human.render_job(job)
    finally:
        human.console = saved
    return buf.getvalue()


def _job_with_results(results: list[TargetResult]) -> Job:
    base = Job.create(
        sha="a" * 40, branch="feat/x",
        target_names=[r.target_name for r in results],
        mode=ValidationMode.FULL,
    ).start()
    for r in results:
        base = base.with_result(r)
    return base.complete()


def test_errored_target_surfaces_error_message() -> None:
    now = datetime.now(timezone.utc)
    errored = TargetResult(
        target_name="ubuntu",
        platform="linux-arm64",
        status=TargetStatus.ERROR,
        backend="ssh",
        started_at=now,
        completed_at=now,
        duration_secs=14.0,
        log_path="/tmp/shipyard/logs/ubuntu.log",
        error_message="Bundle apply failed: fatal: could not read packed object",
    )
    output = _render(_job_with_results([errored]))
    assert "ubuntu" in output
    assert "Bundle apply failed" in output
    assert "/tmp/shipyard/logs/ubuntu.log" in output


def test_passing_target_has_no_error_block() -> None:
    now = datetime.now(timezone.utc)
    ok = TargetResult(
        target_name="mac",
        platform="macos-arm64",
        status=TargetStatus.PASS,
        backend="local",
        started_at=now,
        completed_at=now,
        duration_secs=60.0,
    )
    output = _render(_job_with_results([ok]))
    assert "✗" not in output


def test_error_line_trimmed_when_very_long() -> None:
    now = datetime.now(timezone.utc)
    long_msg = "Bundle apply failed: " + ("x" * 1000)
    errored = TargetResult(
        target_name="windows",
        platform="windows-x64",
        status=TargetStatus.ERROR,
        backend="ssh-windows",
        started_at=now,
        completed_at=now,
        duration_secs=95.0,
        error_message=long_msg,
    )
    output = _render(_job_with_results([errored]))
    assert "windows" in output
    # Trimmed with ellipsis; full message is only in the log file.
    assert "…" in output


def test_failing_target_without_error_message_is_silent() -> None:
    # Status=FAIL with no error_message (e.g. remote exit 1 from a
    # legit test failure) — we don't want a phantom "✗" row; the
    # status cell already reads "fail".
    now = datetime.now(timezone.utc)
    failed = TargetResult(
        target_name="mac",
        platform="macos-arm64",
        status=TargetStatus.FAIL,
        backend="local",
        started_at=now,
        completed_at=now,
        duration_secs=60.0,
    )
    output = _render(_job_with_results([failed]))
    assert "✗ mac" not in output


def test_reused_target_skipped() -> None:
    now = datetime.now(timezone.utc)
    reused = TargetResult(
        target_name="ubuntu",
        platform="linux-arm64",
        status=TargetStatus.PASS,
        backend="reused",
        started_at=now,
        completed_at=now,
        duration_secs=0.0,
        reused_from="abc1234567",
    )
    output = _render(_job_with_results([reused]))
    assert "✗" not in output
