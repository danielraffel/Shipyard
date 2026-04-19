"""Cloud executor — dispatches validation via GitHub Actions workflows.

Triggers a workflow run with `gh workflow run`, then polls for
completion via `gh run list` and `gh run view`. Supports pluggable
runner providers (GitHub-hosted, Namespace) and per-platform runner
selector overrides.
"""

from __future__ import annotations

import json
import subprocess
import time
from datetime import datetime, timezone
from typing import TYPE_CHECKING, Any

from shipyard.core.classify import FailureClass
from shipyard.core.job import TargetResult, TargetStatus

if TYPE_CHECKING:
    from shipyard.executor.streaming import ProgressCallback

# How long to wait between poll attempts (seconds)
_POLL_INTERVAL = 15
# Maximum time to wait for a workflow run to appear in the list
_DISPATCH_SETTLE_SECS = 30
# Maximum time to wait for a workflow run to complete. After this
# the run is reported as ERROR (timeout). Without this bound a
# hanging GitHub Actions workflow would block the drain lock
# indefinitely, blocking every other queued job on the machine.
# Defaults to 60 minutes, which covers the long tail of real-world
# cross-platform builds while still cutting losses on stuck runs.
_MAX_POLL_SECS = 3600


class CloudExecutor:
    """Execute validation by dispatching a GitHub Actions workflow."""

    def __init__(
        self,
        workflow: str = "build.yml",
        repo: str | None = None,
        poll_interval: float = _POLL_INTERVAL,
        dispatch_settle_secs: float = _DISPATCH_SETTLE_SECS,
        max_poll_secs: float = _MAX_POLL_SECS,
    ) -> None:
        self.workflow = workflow
        self.repo = repo
        self.poll_interval = poll_interval
        self.dispatch_settle_secs = dispatch_settle_secs
        self.max_poll_secs = max_poll_secs

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        progress_callback: ProgressCallback | None = None,
        resume_from: str | None = None,  # accepted for API symmetry
        mode: str = "default",            # accepted for API symmetry
    ) -> TargetResult:
        # CloudExecutor does not yet implement stage-aware resume
        # or prepared-state reuse — those are still local-only
        # capabilities. The parameters are accepted so the CLI
        # dispatch path can pass the same kwargs to every backend
        # without raising TypeError.
        del resume_from, mode
        target_name = target_config.get("name", "cloud")
        platform = target_config.get("platform", "unknown")
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        # Resolve runner provider and selector
        runner_provider = target_config.get("runner_provider", "github-hosted")
        runner_profile = target_config.get("runner_selector")

        # Build workflow dispatch inputs
        inputs: dict[str, str] = {
            "ref": branch,
        }
        if runner_provider:
            inputs["runner_provider"] = runner_provider
        if runner_profile:
            inputs["runner_selector"] = runner_profile

        # Per-platform runner selector JSON overrides
        runner_overrides = target_config.get("runner_overrides")
        if runner_overrides:
            inputs["runner_overrides"] = json.dumps(runner_overrides)

        # Dispatch the workflow
        try:
            self._dispatch_workflow(branch, inputs)
            _emit_progress(progress_callback, phase="dispatch")
        except subprocess.CalledProcessError as exc:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="cloud",
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                error_message=f"Failed to dispatch workflow: {exc}",
                provider=runner_provider,
                runner_profile=runner_profile,
                failure_class=FailureClass.INFRA.value,
            )

        # Wait for the run to appear and get its ID
        try:
            run_id = self._wait_for_run(branch)
            _emit_progress(progress_callback, phase="queued")
        except TimeoutError as exc:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="cloud",
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                duration_secs=time.monotonic() - start_time,
                error_message=str(exc),
                provider=runner_provider,
                runner_profile=runner_profile,
                failure_class=FailureClass.TIMEOUT.value,
            )

        # Poll for completion. Per-target override allows long-running
        # workflows (e.g. heavy cross-compile) to extend the cap; the
        # 60-minute default protects the queue from indefinite hangs.
        per_target_max = target_config.get("cloud_max_poll_secs")
        max_poll_secs = (
            float(per_target_max) if per_target_max is not None
            else self.max_poll_secs
        )
        try:
            conclusion = self._poll_run(
                run_id,
                progress_callback=progress_callback,
                max_poll_secs=max_poll_secs,
            )
        except subprocess.CalledProcessError as exc:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="cloud",
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                duration_secs=time.monotonic() - start_time,
                error_message=f"Failed to poll workflow run: {exc}",
                provider=runner_provider,
                runner_profile=runner_profile,
                failure_class=FailureClass.INFRA.value,
            )
        except TimeoutError as exc:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="cloud",
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                duration_secs=time.monotonic() - start_time,
                error_message=str(exc),
                provider=runner_provider,
                runner_profile=runner_profile,
                failure_class=FailureClass.TIMEOUT.value,
            )

        elapsed = time.monotonic() - start_time
        status = TargetStatus.PASS if conclusion == "success" else TargetStatus.FAIL

        # Cloud executor doesn't have local stderr — we can only
        # distinguish "the workflow reported failure" (TEST) from the
        # infra/timeout paths above, which short-circuit before here.
        failure_class = None if status == TargetStatus.PASS else FailureClass.TEST.value

        return TargetResult(
            target_name=target_name,
            platform=platform,
            status=status,
            backend="cloud",
            duration_secs=elapsed,
            started_at=started_at,
            completed_at=datetime.now(timezone.utc),
            log_path=log_path,
            provider=runner_provider,
            runner_profile=runner_profile,
            failure_class=failure_class,
        )

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Check whether gh CLI is authenticated."""
        try:
            result = subprocess.run(
                ["gh", "auth", "status"],
                capture_output=True,
                timeout=10,
            )
            return result.returncode == 0
        except (subprocess.TimeoutExpired, FileNotFoundError):
            return False

    def _repo_args(self) -> list[str]:
        """Return ['--repo', '<repo>'] if a repo is configured."""
        if self.repo:
            return ["--repo", self.repo]
        return []

    def _dispatch_workflow(self, branch: str, inputs: dict[str, str]) -> None:
        """Dispatch a GitHub Actions workflow run."""
        cmd: list[str] = [
            "gh", "workflow", "run", self.workflow,
            "--ref", branch,
        ]
        cmd.extend(self._repo_args())
        for key, value in inputs.items():
            if key != "ref":  # ref is already passed via --ref
                cmd.extend(["-f", f"{key}={value}"])

        subprocess.run(cmd, capture_output=True, check=True, timeout=30)

    def _wait_for_run(self, branch: str) -> str:
        """Wait for the dispatched run to appear and return its ID."""
        deadline = time.monotonic() + self.dispatch_settle_secs
        while time.monotonic() < deadline:
            cmd: list[str] = [
                "gh", "run", "list",
                "--workflow", self.workflow,
                "--branch", branch,
                "--limit", "1",
                "--json", "databaseId,status",
            ]
            cmd.extend(self._repo_args())

            result = subprocess.run(cmd, capture_output=True, text=True, timeout=15)
            if result.returncode == 0 and result.stdout.strip():
                runs = json.loads(result.stdout)
                if runs:
                    return str(runs[0]["databaseId"])

            time.sleep(min(self.poll_interval, 5))

        raise TimeoutError(
            f"Workflow run did not appear within {self.dispatch_settle_secs}s"
        )

    def _poll_run(
        self,
        run_id: str,
        *,
        progress_callback: ProgressCallback | None = None,
        max_poll_secs: float | None = None,
    ) -> str:
        """Poll a workflow run until it completes. Returns the conclusion.

        Raises TimeoutError if the run does not complete within
        ``max_poll_secs``. None means use ``self.max_poll_secs``.
        Pass ``float('inf')`` to disable the bound (e.g. in tests
        that mock a fast completion).
        """
        if max_poll_secs is None:
            max_poll_secs = self.max_poll_secs
        deadline = time.monotonic() + max_poll_secs
        while True:
            cmd: list[str] = [
                "gh", "run", "view", run_id,
                "--json", "status,conclusion",
            ]
            cmd.extend(self._repo_args())

            result = subprocess.run(cmd, capture_output=True, text=True, check=True, timeout=15)
            data = json.loads(result.stdout)
            _emit_progress(
                progress_callback,
                phase=str(data.get("status") or "poll"),
            )

            if data.get("status") == "completed":
                return data.get("conclusion", "failure")

            if time.monotonic() >= deadline:
                raise TimeoutError(
                    f"Cloud workflow run {run_id} did not complete within "
                    f"{int(max_poll_secs)}s"
                )
            time.sleep(self.poll_interval)


def _emit_progress(progress_callback: ProgressCallback | None, *, phase: str) -> None:
    if progress_callback is None:
        return
    now = datetime.now(timezone.utc)
    progress_callback(
        {
            "phase": phase,
            "last_output_at": now,
            "last_heartbeat_at": now,
            "quiet_for_secs": 0.0,
            "liveness": "active",
        }
    )
