"""GitHub CLI helpers for cloud workflow dispatch."""

from __future__ import annotations

import json
import subprocess
import time
from typing import Any


def workflow_dispatch(
    *,
    repository: str | None,
    workflow_file: str,
    ref: str,
    fields: dict[str, str],
) -> None:
    cmd = ["gh", "workflow", "run", workflow_file, "--ref", ref]
    if repository:
        cmd.extend(["--repo", repository])
    for key, value in fields.items():
        cmd.extend(["-f", f"{key}={value}"])
    subprocess.run(cmd, capture_output=True, check=True, timeout=30)


def find_dispatched_run(
    *,
    repository: str | None,
    workflow_file: str,
    ref: str,
    timeout_secs: float = 30.0,
    poll_interval_secs: float = 5.0,
) -> dict[str, Any]:
    deadline = time.monotonic() + timeout_secs
    while time.monotonic() < deadline:
        cmd = [
            "gh",
            "run",
            "list",
            "--workflow",
            workflow_file,
            "--branch",
            ref,
            "--limit",
            "1",
            "--json",
            "databaseId,status,conclusion,url,createdAt,updatedAt,workflowName,headBranch,headSha",
        ]
        if repository:
            cmd.extend(["--repo", repository])

        result = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
        if result.returncode == 0 and result.stdout.strip():
            runs = json.loads(result.stdout)
            if runs:
                return runs[0]
        time.sleep(poll_interval_secs)

    raise TimeoutError(f"Workflow run for '{workflow_file}' on '{ref}' did not appear within {timeout_secs}s")


def run_view(*, repository: str | None, run_id: str) -> dict[str, Any]:
    cmd = [
        "gh",
        "run",
        "view",
        run_id,
        "--json",
        "databaseId,status,conclusion,url,createdAt,updatedAt,workflowName,headBranch,headSha,jobs",
    ]
    if repository:
        cmd.extend(["--repo", repository])
    result = subprocess.run(cmd, capture_output=True, text=True, check=True, timeout=30)
    return json.loads(result.stdout)
