"""SSH Windows executor — runs validation on remote Windows hosts via PowerShell.

Same bundle-based delivery as the POSIX SSH executor, but uses PowerShell
semantics for remote commands and Windows-style paths.
"""

from __future__ import annotations

import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.bundle.git_bundle import create_bundle, upload_bundle
from shipyard.core.job import TargetResult, TargetStatus
from shipyard.executor.streaming import ProgressCallback, run_streaming_command


class SSHWindowsExecutor:
    """Execute validation commands on a remote Windows host via SSH + PowerShell."""

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        progress_callback: ProgressCallback | None = None,
    ) -> TargetResult:
        target_name = target_config.get("name", "windows")
        platform = target_config.get("platform", "windows-x64")
        host = target_config["host"]
        remote_repo = target_config.get("repo_path", "C:\\repo")
        ssh_options = _ssh_options(target_config)
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        # Step 1: Create and deliver git bundle
        with tempfile.TemporaryDirectory() as tmpdir:
            bundle_path = Path(tmpdir) / "shipyard.bundle"
            remote_bundle = target_config.get(
                "remote_bundle_path", "C:\\Temp\\shipyard.bundle"
            )

            bundle_result = create_bundle(
                sha=sha,
                output_path=bundle_path,
                repo_dir=target_config.get("local_repo_dir"),
            )
            if not bundle_result.success:
                return _error_result(
                    target_name, platform, started_at, start_time,
                    str(log_file),
                    f"Bundle creation failed: {bundle_result.message}",
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
                    str(log_file),
                    f"Bundle upload failed: {upload_result.message}",
                )

            # Apply bundle via PowerShell on the remote
            apply_result = _apply_bundle_windows(
                host=host,
                bundle_path=remote_bundle,
                repo_path=remote_repo,
                ssh_options=ssh_options,
            )
            if not apply_result.success:
                return _error_result(
                    target_name, platform, started_at, start_time,
                    str(log_file),
                    f"Bundle apply failed: {apply_result.message}",
                )

        # Step 2: Checkout the SHA and run validation via PowerShell
        command = _build_remote_command(sha, remote_repo, validation_config)
        if not command:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), "No validation command configured",
            )

        ssh_cmd = (
            ["ssh"] + list(ssh_options) + [host, "powershell", "-Command", command]
        )

        try:
            result = run_streaming_command(
                ssh_cmd,
                log_path=str(log_file),
                timeout=target_config.get("timeout_secs", 1800),
                progress_callback=progress_callback,
            )

            status = TargetStatus.PASS if result.returncode == 0 else TargetStatus.FAIL
            error_message = None
            if result.returncode == 255:
                status = TargetStatus.ERROR
                error_message = _extract_ssh_error(result.output) or "SSH transport failed"

            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=status,
                backend="ssh-windows",
                duration_secs=result.duration_secs,
                started_at=started_at,
                completed_at=result.completed_at,
                log_path=str(log_file),
                phase=result.phase,
                last_output_at=result.last_output_at,
                error_message=error_message,
            )

        except subprocess.TimeoutExpired:
            elapsed = time.monotonic() - start_time
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="ssh-windows",
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
        """Check SSH reachability with a PowerShell echo command."""
        host = target_config.get("host")
        if not host:
            return False

        ssh_options = _ssh_options(target_config)
        cmd = (
            ["ssh"]
            + list(ssh_options)
            + ["-o", "ConnectTimeout=5", host, "powershell", "-Command", "Write-Output ok"]
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


class _ApplyResult:
    """Minimal result type for the internal apply operation."""

    def __init__(self, success: bool, message: str) -> None:
        self.success = success
        self.message = message


def _apply_bundle_windows(
    host: str,
    bundle_path: str,
    repo_path: str,
    ssh_options: list[str],
) -> _ApplyResult:
    """Apply a git bundle on a remote Windows host via PowerShell."""
    ps_cmd = (
        f"cd '{repo_path}'; "
        f"git bundle verify '{bundle_path}'; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}; "
        f"git fetch '{bundle_path}' '+refs/*:refs/*'; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}"
    )

    cmd = ["ssh"] + list(ssh_options) + [host, "powershell", "-Command", ps_cmd]

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode != 0:
            return _ApplyResult(
                success=False,
                message=f"Remote bundle apply failed: {result.stderr.strip()}",
            )
        return _ApplyResult(success=True, message="Bundle applied")

    except subprocess.TimeoutExpired:
        return _ApplyResult(success=False, message="Remote bundle apply timed out")
    except OSError as exc:
        return _ApplyResult(success=False, message=f"OS error: {exc}")


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
    """Build the remote PowerShell command: cd + checkout + validate.

    Uses semicolons for PowerShell command chaining with $LASTEXITCODE checks.
    """
    if "command" in validation_config:
        validate_cmd = validation_config["command"]
    else:
        parts: list[str] = []
        for step in ("setup", "configure", "build", "test"):
            cmd = validation_config.get(step)
            if cmd:
                parts.append(f"Write-Output '__SHIPYARD_PHASE__:{step}'; {cmd}")
        if not parts:
            return None
        # Chain with PowerShell error checking
        validate_cmd = "; if ($LASTEXITCODE -ne 0) { exit 1 }; ".join(parts)

    return (
        f"cd '{remote_repo}'; "
        f"git checkout --force {sha}; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}; "
        f"{validate_cmd}"
    )


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
        backend="ssh-windows",
        duration_secs=time.monotonic() - start_time,
        started_at=started_at,
        completed_at=datetime.now(timezone.utc),
        log_path=log_path,
        error_message=message,
    )


def _extract_ssh_error(output: str) -> str | None:
    for line in reversed(output.splitlines()):
        if line.strip():
            return line.strip()
    return None
