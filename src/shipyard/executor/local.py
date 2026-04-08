"""Local executor — runs validation on the current machine.

This is the simplest executor: shell out to the validation command
in a clean worktree, capture output, return pass/fail.
"""

from __future__ import annotations

import subprocess
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.core.job import TargetResult, TargetStatus


class LocalExecutor:
    """Execute validation commands locally via subprocess."""

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
    ) -> TargetResult:
        target_name = target_config.get("name", "local")
        platform = target_config.get("platform", "unknown")
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        # Build the validation command
        command = _build_command(validation_config)
        if not command:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="local",
                error_message="No validation command configured",
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
            )

        # Ensure log directory exists
        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        try:
            with open(log_file, "w") as log:
                result = subprocess.run(
                    command,
                    shell=True,
                    cwd=target_config.get("cwd"),
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    timeout=target_config.get("timeout_secs", 1800),  # 30 min default
                )

            elapsed = time.monotonic() - start_time
            status = TargetStatus.PASS if result.returncode == 0 else TargetStatus.FAIL

            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=status,
                backend="local",
                duration_secs=elapsed,
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
            )

        except subprocess.TimeoutExpired:
            elapsed = time.monotonic() - start_time
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="local",
                duration_secs=elapsed,
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
                error_message="Validation timed out",
            )

        except OSError as exc:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="local",
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
                error_message=str(exc),
            )

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Local target is always reachable."""
        return True


def _build_command(validation_config: dict[str, Any]) -> str | None:
    """Assemble the validation command from config sections.

    Supports either a single 'command' field or separate
    'configure' / 'build' / 'test' fields chained with &&.
    """
    if "command" in validation_config:
        return validation_config["command"]

    parts: list[str] = []
    for step in ("setup", "configure", "build", "test"):
        cmd = validation_config.get(step)
        if cmd:
            parts.append(cmd)

    return " && ".join(parts) if parts else None
