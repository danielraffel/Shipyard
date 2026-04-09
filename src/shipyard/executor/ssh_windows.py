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
from shipyard.executor.contract import evaluate_contract, required_markers
from shipyard.executor.streaming import ProgressCallback, run_streaming_command
from shipyard.executor.windows_toolchain import (
    DEFAULT_MUTEX_NAME,
    VsToolchain,
    detect_vs_toolchain,
    toolchain_env_exports,
    wrap_powershell_with_host_mutex,
)


class SSHWindowsExecutor:
    """Execute validation commands on a remote Windows host via SSH + PowerShell."""

    def __init__(self) -> None:
        # Cache of detected VS toolchains keyed by host. Detection
        # shells out to vswhere, which is slow enough (~1s) that we
        # only want to do it once per Shipyard invocation per host.
        self._vs_toolchain_cache: dict[str, VsToolchain | None] = {}

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

        # Step 2: Resolve the Visual Studio toolchain (once per host,
        # cached) so stages can reference $env:SHIPYARD_CMAKE_PLATFORM
        # and $env:SHIPYARD_CMAKE_GENERATOR_INSTANCE. This matches
        # Pulp's local_ci.py behavior for hosts with multiple VS
        # installations or ARM64 Windows. Detection is best-effort:
        # a None result just means CMake falls back to its defaults.
        toolchain = self._get_vs_toolchain(host, ssh_options, target_config)

        # Step 3: Build the remote PowerShell command: env exports +
        # checkout + validate. If the target opts into a host mutex,
        # wrap the whole thing in a Mutex block so concurrent runs
        # against the same Windows host queue up instead of racing.
        command = _build_remote_command(
            sha, remote_repo, validation_config, toolchain=toolchain,
        )
        if not command:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), "No validation command configured",
            )

        if _host_mutex_enabled(target_config):
            command = wrap_powershell_with_host_mutex(
                command,
                mutex_name=target_config.get(
                    "windows_host_mutex_name", DEFAULT_MUTEX_NAME,
                ),
            )

        ssh_cmd = (
            ["ssh"] + list(ssh_options) + [host, "powershell", "-Command", command]
        )

        contract_config = validation_config.get("contract") if validation_config else None

        try:
            result = run_streaming_command(
                ssh_cmd,
                log_path=str(log_file),
                timeout=target_config.get("timeout_secs", 1800),
                progress_callback=progress_callback,
                required_contract_markers=required_markers(contract_config),
            )

            status = TargetStatus.PASS if result.returncode == 0 else TargetStatus.FAIL
            error_message = None
            if result.returncode == 255:
                status = TargetStatus.ERROR
                error_message = _extract_ssh_error(result.output) or "SSH transport failed"

            evaluation = evaluate_contract(contract_config, result.contract_markers_seen)
            if evaluation.should_force_fail and status == TargetStatus.PASS:
                status = TargetStatus.FAIL
                error_message = evaluation.message

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
                contract_markers_seen=evaluation.seen,
                contract_markers_missing=evaluation.missing,
                contract_violation=evaluation.message,
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

    def _get_vs_toolchain(
        self,
        host: str,
        ssh_options: list[str],
        target_config: dict[str, Any],
    ) -> VsToolchain | None:
        """Return the cached or freshly-detected VS toolchain for a host.

        Respects `windows_vs_detect = false` in the target config to
        opt out entirely. Detection failures are cached as None so a
        missing vswhere doesn't re-probe on every run.
        """
        if target_config.get("windows_vs_detect", True) is False:
            return None
        if host in self._vs_toolchain_cache:
            return self._vs_toolchain_cache[host]
        toolchain = detect_vs_toolchain(host, ssh_options)
        self._vs_toolchain_cache[host] = toolchain
        return toolchain

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
    *,
    toolchain: VsToolchain | None = None,
) -> str | None:
    """Build the remote PowerShell command: cd + checkout + validate.

    Uses semicolons for PowerShell command chaining with $LASTEXITCODE checks.
    When `toolchain` is provided, the resolved CMake platform and generator
    instance are exported as env vars so stages can reference them.
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
        f"{toolchain_env_exports(toolchain)}; "
        f"cd '{remote_repo}'; "
        f"git checkout --force {sha}; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}; "
        f"{validate_cmd}"
    )


def _host_mutex_enabled(target_config: dict[str, Any]) -> bool:
    """Read the host-mutex opt-in from the target config.

    Default: True. Concurrent validation runs on a Windows host share
    a checked-out repo and a locked VS install, so serializing by
    default prevents hard-to-diagnose flaky failures. Projects that
    use per-job worktrees can disable it with
    `windows_host_mutex = false` in the target config.
    """
    return target_config.get("windows_host_mutex", True) is not False


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
