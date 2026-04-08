"""SSH POSIX executor — runs validation on remote Linux/macOS hosts.

Delivers code via git bundle, then runs the validation command over SSH.
Captures output to a local log file for later inspection.
"""

from __future__ import annotations

import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.bundle.git_bundle import apply_bundle, create_bundle, upload_bundle
from shipyard.core.job import TargetResult, TargetStatus


class SSHExecutor:
    """Execute validation commands on a remote POSIX host via SSH."""

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
    ) -> TargetResult:
        target_name = target_config.get("name", "ssh")
        platform = target_config.get("platform", "unknown")
        host = target_config["host"]
        remote_repo = target_config.get("repo_path", "~/repo")
        ssh_options = _ssh_options(target_config)
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        # Step 1: Create and deliver git bundle
        with tempfile.TemporaryDirectory() as tmpdir:
            bundle_path = Path(tmpdir) / "shipyard.bundle"
            remote_bundle = target_config.get(
                "remote_bundle_path", "/tmp/shipyard.bundle"
            )

            bundle_result = create_bundle(
                sha=sha,
                output_path=bundle_path,
                repo_dir=target_config.get("local_repo_dir"),
            )
            if not bundle_result.success:
                return _error_result(
                    target_name, platform, started_at, start_time,
                    str(log_file), f"Bundle creation failed: {bundle_result.message}",
                )

            upload_result = upload_bundle(
                bundle_path=bundle_path,
                host=host,
                remote_path=remote_bundle,
                ssh_options=ssh_options,
            )
            if not upload_result.success:
                return _error_result(
                    target_name, platform, started_at, start_time,
                    str(log_file), f"Bundle upload failed: {upload_result.message}",
                )

            apply_result = apply_bundle(
                host=host,
                bundle_path=remote_bundle,
                repo_path=remote_repo,
                ssh_options=ssh_options,
            )
            if not apply_result.success:
                return _error_result(
                    target_name, platform, started_at, start_time,
                    str(log_file), f"Bundle apply failed: {apply_result.message}",
                )

        # Step 2: Checkout the SHA and run validation
        command = _build_remote_command(sha, remote_repo, validation_config)
        if not command:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), "No validation command configured",
            )

        ssh_cmd = ["ssh"] + list(ssh_options) + [host, command]

        try:
            with open(log_file, "w") as log:
                result = subprocess.run(
                    ssh_cmd,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    timeout=target_config.get("timeout_secs", 1800),
                )

            elapsed = time.monotonic() - start_time
            status = TargetStatus.PASS if result.returncode == 0 else TargetStatus.FAIL

            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=status,
                backend="ssh",
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
                backend="ssh",
                duration_secs=elapsed,
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
                error_message="Validation timed out",
            )

        except OSError as exc:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), str(exc),
            )

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Check SSH reachability with a quick echo command."""
        host = target_config.get("host")
        if not host:
            return False

        ssh_options = _ssh_options(target_config)
        cmd = (
            ["ssh"]
            + list(ssh_options)
            + ["-o", "ConnectTimeout=5", host, "echo ok"]
        )

        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=10,
            )
            return result.returncode == 0
        except (subprocess.TimeoutExpired, OSError):
            return False


def _ssh_options(target_config: dict[str, Any]) -> list[str]:
    """Extract SSH options from target config."""
    options: list[str] = []
    if "ssh_options" in target_config:
        options.extend(target_config["ssh_options"])
    if "identity_file" in target_config:
        options.extend(["-i", target_config["identity_file"]])
    return options


def _build_remote_command(
    sha: str,
    remote_repo: str,
    validation_config: dict[str, Any],
) -> str | None:
    """Build the remote shell command: checkout + validate."""
    if "command" in validation_config:
        validate_cmd = validation_config["command"]
    else:
        parts: list[str] = []
        for step in ("setup", "configure", "build", "test"):
            cmd = validation_config.get(step)
            if cmd:
                parts.append(cmd)
        if not parts:
            return None
        validate_cmd = " && ".join(parts)

    return f"cd {remote_repo} && git checkout --force {sha} && {validate_cmd}"


def _error_result(
    target_name: str,
    platform: str,
    started_at: datetime,
    start_time: float,
    log_path: str,
    message: str,
) -> TargetResult:
    """Create an ERROR TargetResult."""
    return TargetResult(
        target_name=target_name,
        platform=platform,
        status=TargetStatus.ERROR,
        backend="ssh",
        duration_secs=time.monotonic() - start_time,
        started_at=started_at,
        completed_at=datetime.now(timezone.utc),
        log_path=log_path,
        error_message=message,
    )
