"""Backend-aware executor dispatch for CLI commands."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, cast

from shipyard.executor.cloud import CloudExecutor
from shipyard.executor.local import LocalExecutor
from shipyard.executor.ssh import SSHExecutor
from shipyard.executor.ssh_windows import SSHWindowsExecutor
from shipyard.failover.chain import FallbackChain, FallbackExecutor

if TYPE_CHECKING:
    from shipyard.core.job import TargetResult

_WINDOWS_BACKENDS = {"ssh-windows", "ssh_windows"}


class ExecutorDispatcher:
    """Resolve target configs to concrete executor implementations."""

    def __init__(
        self,
        *,
        cloud_workflow: str = "ci.yml",
        cloud_repo: str | None = None,
        cloud_poll_interval: float = 15.0,
        cloud_dispatch_settle_secs: float = 30.0,
    ) -> None:
        self.cloud_workflow = cloud_workflow
        self.cloud_repo = cloud_repo
        self.cloud_poll_interval = cloud_poll_interval
        self.cloud_dispatch_settle_secs = cloud_dispatch_settle_secs
        self._local = LocalExecutor()
        self._ssh = SSHExecutor()
        self._ssh_windows = SSHWindowsExecutor()

    def executor_for(self, target_config: dict[str, Any]) -> Any:
        fallback = list(target_config.get("fallback", []))
        if fallback:
            primary = _primary_backend_def(target_config)
            backends = [primary, *fallback]
            types = {_normalize_backend_name(backend) for backend in backends}
            executors = {backend_type: self for backend_type in types if backend_type != "vm"}
            return FallbackChain(
                backends=backends,
                executors=cast("dict[str, FallbackExecutor]", executors),
            )
        return self

    def validate_target(
        self,
        *,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        **kwargs: Any,
    ) -> TargetResult:
        executor = self.executor_for(target_config)
        if isinstance(executor, FallbackChain):
            return executor.execute(
                job_sha=sha,
                job_branch=branch,
                target_config=target_config,
                validation_config=validation_config,
                log_path=log_path,
            )
        return executor.validate(
            sha=sha,
            branch=branch,
            target_config=target_config,
            validation_config=validation_config,
            log_path=log_path,
            **kwargs,
        )

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        **kwargs: Any,
    ) -> TargetResult:  # type: ignore[override]
        executor = self._resolve_executor(target_config)
        return executor.validate(
            sha=sha,
            branch=branch,
            target_config=target_config,
            validation_config=validation_config,
            log_path=log_path,
            **kwargs,
        )

    def probe(self, target_config: dict[str, Any]) -> bool:
        executor = self._resolve_executor(target_config)
        return executor.probe(target_config)

    def backend_name(self, target_config: dict[str, Any]) -> str:
        return _normalize_backend_name(target_config)

    def _resolve_executor(self, target_config: dict[str, Any]) -> Any:
        backend = _normalize_backend_name(target_config)
        if backend == "local":
            return self._local
        if backend == "ssh":
            return self._ssh
        if backend in _WINDOWS_BACKENDS:
            return self._ssh_windows
        if backend == "cloud":
            return CloudExecutor(
                workflow=target_config.get("workflow", self.cloud_workflow),
                repo=target_config.get("repository", self.cloud_repo),
                poll_interval=float(target_config.get("poll_interval_secs", self.cloud_poll_interval)),
                dispatch_settle_secs=float(
                    target_config.get("dispatch_settle_secs", self.cloud_dispatch_settle_secs)
                ),
            )
        raise ValueError(f"Unsupported backend '{backend}'")


def _primary_backend_def(target_config: dict[str, Any]) -> dict[str, Any]:
    backend = _normalize_backend_name(target_config)
    primary = dict(target_config)
    primary["type"] = backend
    return primary


def _normalize_backend_name(target_config: dict[str, Any]) -> str:
    backend = str(target_config.get("type") or target_config.get("backend") or "local").strip().lower()
    backend = backend.replace("_", "-")
    if backend == "ssh" and str(target_config.get("platform", "")).startswith("windows"):
        return "ssh-windows"
    return backend
