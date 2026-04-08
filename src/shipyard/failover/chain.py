"""Failover chain — tries backends in priority order.

A FallbackChain takes an ordered list of backend definitions and
attempts validation on each in sequence. If the primary fails with
an infrastructure error (not a test failure), the next backend is
tried. Test failures are final — they indicate real problems, not
infrastructure issues.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass
from typing import Any, Protocol

from shipyard.core.job import TargetResult, TargetStatus

logger = logging.getLogger(__name__)


class FallbackExecutor(Protocol):
    """Minimal executor interface for fallback chain entries."""

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
    ) -> TargetResult: ...

    def probe(self, target_config: dict[str, Any]) -> bool: ...


# Statuses that indicate infrastructure problems (worth retrying on another backend)
_RETRIABLE_STATUSES = frozenset({TargetStatus.ERROR, TargetStatus.UNREACHABLE})


@dataclass
class FallbackChain:
    """Ordered list of backends to try for a target.

    Each entry in `backends` is a structured dict describing the backend:
        {"type": "vm", "vm_name": "Ubuntu 24.04"}
        {"type": "cloud", "provider": "namespace"}
        {"type": "local"}
        {"type": "ssh", "host": "ubuntu"}

    The `executors` dict maps backend type strings to executor instances.
    """

    backends: list[dict[str, Any]]
    executors: dict[str, FallbackExecutor]

    def execute(
        self,
        job_sha: str,
        job_branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
    ) -> TargetResult:
        """Try each backend in order until one succeeds or all fail.

        - PASS/FAIL results are terminal (returned immediately).
        - ERROR/UNREACHABLE results trigger failover to the next backend.
        - The final result includes provenance about which backend was
          primary and why failover occurred.

        Args:
            job_sha: Commit SHA to validate.
            job_branch: Branch name.
            target_config: Target definition.
            validation_config: Validation commands.
            log_path: Base log path (suffixed per attempt).

        Returns:
            TargetResult with failover provenance when applicable.
        """
        if not self.backends:
            target_name = target_config.get("name", "unknown")
            platform = target_config.get("platform", "unknown")
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="none",
                error_message="No backends configured in fallback chain",
            )

        primary_type = _backend_label(self.backends[0])
        last_result: TargetResult | None = None

        for i, backend_def in enumerate(self.backends):
            backend_type = backend_def.get("type", "unknown")
            executor = self.executors.get(backend_type)

            if executor is None:
                logger.warning(
                    "No executor registered for backend type '%s', skipping",
                    backend_type,
                )
                continue

            # Probe before attempting validation
            if not executor.probe(target_config):
                logger.info(
                    "Backend '%s' probe failed, trying next",
                    _backend_label(backend_def),
                )
                target_name = target_config.get("name", "unknown")
                platform = target_config.get("platform", "unknown")
                last_result = TargetResult(
                    target_name=target_name,
                    platform=platform,
                    status=TargetStatus.UNREACHABLE,
                    backend=_backend_label(backend_def),
                    error_message=f"Probe failed for {_backend_label(backend_def)}",
                )
                continue

            # Build per-attempt log path
            attempt_log = f"{log_path}.attempt-{i}" if i > 0 else log_path

            # Merge backend-specific config into target_config
            merged_config = {**target_config, **backend_def}

            result = executor.validate(
                sha=job_sha,
                branch=job_branch,
                target_config=merged_config,
                validation_config=validation_config,
                log_path=attempt_log,
            )

            # Test failures are authoritative — don't retry
            if result.status == TargetStatus.FAIL:
                return result

            # Success — return with provenance if we failed over
            if result.status == TargetStatus.PASS:
                if i > 0:
                    return TargetResult(
                        target_name=result.target_name,
                        platform=result.platform,
                        status=result.status,
                        backend=f"{_backend_label(backend_def)}-failover",
                        duration_secs=result.duration_secs,
                        started_at=result.started_at,
                        completed_at=result.completed_at,
                        log_path=result.log_path,
                        primary_backend=primary_type,
                        failover_reason=last_result.error_message if last_result else "unknown",
                        provider=result.provider,
                        runner_profile=result.runner_profile,
                    )
                return result

            # Infrastructure error — record and try next
            last_result = result
            logger.info(
                "Backend '%s' returned %s: %s — trying next",
                _backend_label(backend_def),
                result.status.value,
                result.error_message or "no detail",
            )

        # All backends exhausted
        if last_result is not None:
            return TargetResult(
                target_name=last_result.target_name,
                platform=last_result.platform,
                status=last_result.status,
                backend=f"{primary_type}-exhausted",
                duration_secs=last_result.duration_secs,
                started_at=last_result.started_at,
                completed_at=last_result.completed_at,
                log_path=last_result.log_path,
                primary_backend=primary_type,
                failover_reason="All backends exhausted",
                error_message=last_result.error_message,
            )

        target_name = target_config.get("name", "unknown")
        platform = target_config.get("platform", "unknown")
        return TargetResult(
            target_name=target_name,
            platform=platform,
            status=TargetStatus.ERROR,
            backend=f"{primary_type}-exhausted",
            primary_backend=primary_type,
            failover_reason="No usable executors found",
            error_message="All backends skipped (no matching executors)",
        )


def _backend_label(backend_def: dict[str, Any]) -> str:
    """Human-readable label for a backend definition."""
    btype = backend_def.get("type", "unknown")
    if btype == "vm":
        return f"vm:{backend_def.get('vm_name', '?')}"
    if btype == "cloud":
        return f"cloud:{backend_def.get('provider', '?')}"
    if btype == "ssh":
        return f"ssh:{backend_def.get('host', '?')}"
    return btype
