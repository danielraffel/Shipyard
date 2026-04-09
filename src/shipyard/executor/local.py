"""Local executor — runs validation on the current machine.

This is the simplest executor: shell out to the validation command
in a clean worktree, capture output, return pass/fail.

Supports two modes:
- Single command: run one shell command, check exit code
- Stage-aware: run configure/build/test as separate steps, report
  which stage failed, and enable resume from the last successful stage
"""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.core.job import TargetResult, TargetStatus

STAGES = ("setup", "configure", "build", "test")


@dataclass(frozen=True)
class StageResult:
    """Result of running a single validation stage."""

    stage: str
    success: bool
    duration_secs: float
    error_message: str | None = None


class LocalExecutor:
    """Execute validation commands locally via subprocess."""

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        resume_from: str | None = None,
    ) -> TargetResult:
        target_name = target_config.get("name", "local")
        platform = target_config.get("platform", "unknown")
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        # Single command mode
        if "command" in validation_config:
            return self._run_single(
                validation_config["command"], target_name, platform,
                target_config, log_file, started_at, start_time,
            )

        # Stage-aware mode
        stages = _get_stages(validation_config, resume_from)
        if not stages:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                error_message="No validation command configured",
                started_at=started_at, completed_at=datetime.now(timezone.utc),
            )

        return self._run_stages(
            stages, target_name, platform, target_config,
            log_file, started_at, start_time,
        )

    def _run_single(
        self, command: str, target_name: str, platform: str,
        target_config: dict[str, Any], log_file: Path,
        started_at: datetime, start_time: float,
    ) -> TargetResult:
        try:
            with open(log_file, "w") as log:
                result = subprocess.run(
                    command, shell=True, cwd=target_config.get("cwd"),
                    stdout=log, stderr=subprocess.STDOUT,
                    timeout=target_config.get("timeout_secs", 1800),
                )
            elapsed = time.monotonic() - start_time
            status = TargetStatus.PASS if result.returncode == 0 else TargetStatus.FAIL
            return TargetResult(
                target_name=target_name, platform=platform,
                status=status, backend="local", duration_secs=elapsed,
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
            )
        except subprocess.TimeoutExpired:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                duration_secs=time.monotonic() - start_time,
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message="Validation timed out",
            )
        except OSError as exc:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message=str(exc),
            )

    def _run_stages(
        self, stages: list[tuple[str, str]], target_name: str,
        platform: str, target_config: dict[str, Any], log_file: Path,
        started_at: datetime, start_time: float,
    ) -> TargetResult:
        """Run validation as separate stages. Stop at first failure."""
        failed_stage = None
        stage_results: list[StageResult] = []

        try:
            with open(log_file, "w") as log:
                for stage_name, command in stages:
                    stage_start = time.monotonic()
                    log.write(f"\n=== {stage_name} ===\n")
                    log.flush()

                    result = subprocess.run(
                        command, shell=True, cwd=target_config.get("cwd"),
                        stdout=log, stderr=subprocess.STDOUT,
                        timeout=target_config.get("timeout_secs", 1800),
                    )

                    sr = StageResult(
                        stage=stage_name,
                        success=result.returncode == 0,
                        duration_secs=time.monotonic() - stage_start,
                    )
                    stage_results.append(sr)

                    if result.returncode != 0:
                        failed_stage = stage_name
                        break

        except subprocess.TimeoutExpired:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                duration_secs=time.monotonic() - start_time,
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message="Validation timed out",
            )
        except OSError as exc:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message=str(exc),
            )

        elapsed = time.monotonic() - start_time
        if failed_stage:
            error_msg = f"Stage '{failed_stage}' failed"
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.FAIL, backend="local",
                duration_secs=elapsed, started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message=error_msg,
            )

        return TargetResult(
            target_name=target_name, platform=platform,
            status=TargetStatus.PASS, backend="local",
            duration_secs=elapsed, started_at=started_at,
            completed_at=datetime.now(timezone.utc),
            log_path=str(log_file),
        )

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Local target is always reachable."""
        return True


def _get_stages(
    validation_config: dict[str, Any], resume_from: str | None = None
) -> list[tuple[str, str]]:
    """Extract stages from config, optionally skipping to resume_from.

    When resume_from is set (e.g., "test"), earlier stages that already
    passed are skipped. This enables prepared-state resume: if the build
    succeeded but tests failed, you can re-run from "test" without
    rebuilding.
    """
    stages: list[tuple[str, str]] = []
    skipping = resume_from is not None

    for stage_name in STAGES:
        cmd = validation_config.get(stage_name)
        if not cmd:
            continue
        if skipping:
            if stage_name == resume_from:
                skipping = False
            else:
                continue
        stages.append((stage_name, cmd))

    return stages
