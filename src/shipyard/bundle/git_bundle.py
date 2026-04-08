"""Git bundle operations for delivering code to remote hosts.

Creates a git bundle from the local repo, uploads it via SCP, and applies
it on the remote side. This avoids needing the remote to have git credentials
or access to the upstream repository.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


@dataclass(frozen=True)
class BundleResult:
    """Outcome of a bundle operation."""

    success: bool
    message: str
    path: str | None = None


def create_bundle(
    sha: str,
    output_path: str | Path,
    repo_dir: str | Path | None = None,
) -> BundleResult:
    """Create a git bundle containing the given SHA and its ancestors.

    Args:
        sha: The commit SHA to include (up to and including this commit).
        output_path: Local filesystem path for the generated .bundle file.
        repo_dir: Working directory for git commands. Defaults to cwd.

    Returns:
        BundleResult indicating success or failure.
    """
    output_path = Path(output_path)
    output_path.parent.mkdir(parents=True, exist_ok=True)

    cwd = str(repo_dir) if repo_dir else None

    try:
        result = subprocess.run(
            ["git", "bundle", "create", str(output_path), sha, "--all"],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode != 0:
            return BundleResult(
                success=False,
                message=f"git bundle create failed: {result.stderr.strip()}",
            )
        return BundleResult(
            success=True,
            message="Bundle created",
            path=str(output_path),
        )

    except subprocess.TimeoutExpired:
        return BundleResult(success=False, message="git bundle create timed out")
    except OSError as exc:
        return BundleResult(success=False, message=f"OS error: {exc}")


def upload_bundle(
    bundle_path: str | Path,
    host: str,
    remote_path: str,
    ssh_options: Sequence[str] = (),
) -> BundleResult:
    """Upload a bundle file to a remote host via SCP.

    Args:
        bundle_path: Local path to the .bundle file.
        host: SSH host (user@host or alias).
        remote_path: Destination path on the remote host.
        ssh_options: Additional SSH/SCP options (e.g. ["-o", "StrictHostKeyChecking=no"]).

    Returns:
        BundleResult indicating success or failure.
    """
    bundle_path = Path(bundle_path)
    if not bundle_path.exists():
        return BundleResult(
            success=False,
            message=f"Bundle file not found: {bundle_path}",
        )

    cmd: list[str] = ["scp"]
    for opt in ssh_options:
        cmd.append(opt)
    cmd.extend([str(bundle_path), f"{host}:{remote_path}"])

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=300,
        )
        if result.returncode != 0:
            return BundleResult(
                success=False,
                message=f"scp failed: {result.stderr.strip()}",
            )
        return BundleResult(
            success=True,
            message="Bundle uploaded",
            path=remote_path,
        )

    except subprocess.TimeoutExpired:
        return BundleResult(success=False, message="scp timed out")
    except OSError as exc:
        return BundleResult(success=False, message=f"OS error: {exc}")


def apply_bundle(
    host: str,
    bundle_path: str,
    repo_path: str,
    ssh_options: Sequence[str] = (),
) -> BundleResult:
    """Apply a git bundle on a remote host via SSH.

    Fetches all refs from the bundle into the remote repo, then checks out
    the desired state.

    Args:
        host: SSH host (user@host or alias).
        bundle_path: Path to the .bundle file on the remote host.
        repo_path: Path to the git repo on the remote host.
        ssh_options: Additional SSH options.

    Returns:
        BundleResult indicating success or failure.
    """
    remote_cmd = (
        f"cd {repo_path} && "
        f"git bundle verify {bundle_path} && "
        f"git fetch {bundle_path} '+refs/*:refs/*'"
    )

    cmd: list[str] = ["ssh"]
    for opt in ssh_options:
        cmd.append(opt)
    cmd.extend([host, remote_cmd])

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode != 0:
            return BundleResult(
                success=False,
                message=f"Remote bundle apply failed: {result.stderr.strip()}",
            )
        return BundleResult(
            success=True,
            message="Bundle applied",
            path=bundle_path,
        )

    except subprocess.TimeoutExpired:
        return BundleResult(success=False, message="Remote bundle apply timed out")
    except OSError as exc:
        return BundleResult(success=False, message=f"OS error: {exc}")
