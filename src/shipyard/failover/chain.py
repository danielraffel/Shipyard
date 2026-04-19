"""Failover chain — tries backends in priority order.

A FallbackChain takes an ordered list of backend definitions and
attempts validation on each in sequence. If the primary fails with
an infrastructure error (not a test failure), the next backend is
tried. Test failures are final — they indicate real problems, not
infrastructure issues.

Locality routing
----------------

Targets can declare ``requires = ["gpu", "arm64", ...]`` to constrain
which providers in the fallback chain are eligible. Before attempting
any backend, the chain filters itself down to the subset whose
:class:`~shipyard.providers.base.ProviderProfile` satisfies every
requirement. If the filtered chain is empty, the target fails with a
clear error naming the requirements and the profiles that were tried.

Backward compatibility: a target with no ``requires`` (or an empty
list) is not filtered — every backend still runs in order, exactly as
before.
"""

from __future__ import annotations

import logging
from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any, Protocol

from shipyard.core.classify import FailureClass
from shipyard.core.job import TargetResult, TargetStatus

if TYPE_CHECKING:
    from shipyard.providers.base import ProviderProfile

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


class CapabilityMismatchError(ValueError):
    """Raised when no backend in a chain satisfies a target's ``requires``."""


@dataclass
class FallbackChain:
    """Ordered list of backends to try for a target.

    Each entry in `backends` is a structured dict describing the backend:
        {"type": "vm", "vm_name": "Ubuntu 24.04"}
        {"type": "cloud", "provider": "namespace"}
        {"type": "cloud", "provider": "namespace", "profile": "gpu"}
        {"type": "local"}
        {"type": "ssh", "host": "ubuntu", "capabilities": ["gpu", "x86_64"]}

    The `executors` dict maps backend type strings to executor instances.

    The optional ``profiles`` map is a registry of
    ``provider-name -> {profile-name: ProviderProfile}`` used by the
    locality-routing filter. It's supplied by the caller (usually built
    from the provider registry merged with project config). Backends
    that declare capabilities inline don't need an entry here.
    """

    backends: list[dict[str, Any]]
    executors: dict[str, FallbackExecutor]
    profiles: dict[str, dict[str, ProviderProfile]] = field(default_factory=dict)

    def execute(
        self,
        job_sha: str,
        job_branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        **kwargs: Any,
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
        target_name = target_config.get("name", "unknown")
        platform = target_config.get("platform", "unknown")

        if not self.backends:
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="none",
                error_message="No backends configured in fallback chain",
                failure_class=FailureClass.UNKNOWN.value,
            )

        # Apply locality-routing filter before any probing/validation.
        requires = _normalize_requires(target_config.get("requires"))
        filtered_backends = filter_backends_by_requires(
            self.backends, requires, self.profiles
        )
        if not filtered_backends:
            tried = [_profile_label(b, self.profiles) for b in self.backends]
            msg = (
                f"no provider satisfies requires={sorted(requires)}: "
                f"tried [{', '.join(tried)}]"
            )
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend=_backend_label(self.backends[0]),
                error_message=msg,
                failure_class=FailureClass.INFRA.value,
            )

        primary_type = _backend_label(filtered_backends[0])
        last_result: TargetResult | None = None

        for i, backend_def in enumerate(filtered_backends):
            backend_type = backend_def.get("type", "unknown")
            executor = self.executors.get(backend_type)

            if executor is None:
                logger.warning(
                    "No executor registered for backend type '%s', skipping",
                    backend_type,
                )
                continue

            # Merge backend-specific config into the target before probing or validating.
            merged_config = {**target_config, **backend_def}

            # Probe before attempting validation
            if not executor.probe(merged_config):
                logger.info(
                    "Backend '%s' probe failed, trying next",
                    _backend_label(backend_def),
                )
                last_result = TargetResult(
                    target_name=target_name,
                    platform=platform,
                    status=TargetStatus.UNREACHABLE,
                    backend=_backend_label(backend_def),
                    error_message=f"Probe failed for {_backend_label(backend_def)}",
                    failure_class=FailureClass.INFRA.value,
                )
                continue

            # Build per-attempt log path
            attempt_log = f"{log_path}.attempt-{i}" if i > 0 else log_path

            result = executor.validate(
                sha=job_sha,
                branch=job_branch,
                target_config=merged_config,
                validation_config=validation_config,
                log_path=attempt_log,
                **kwargs,
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
                        failure_class=result.failure_class,
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
                failure_class=last_result.failure_class or FailureClass.INFRA.value,
            )

        return TargetResult(
            target_name=target_name,
            platform=platform,
            status=TargetStatus.ERROR,
            backend=f"{primary_type}-exhausted",
            primary_backend=primary_type,
            failover_reason="No usable executors found",
            error_message="All backends skipped (no matching executors)",
            failure_class=FailureClass.UNKNOWN.value,
        )


def filter_backends_by_requires(
    backends: list[dict[str, Any]],
    requires: list[str] | frozenset[str],
    profiles: dict[str, dict[str, ProviderProfile]] | None = None,
) -> list[dict[str, Any]]:
    """Return the subset of ``backends`` that satisfy ``requires``.

    Capability resolution for each backend, in order:

    1. If the backend dict has an explicit ``capabilities`` list, use it.
    2. If the backend has ``type == "cloud"``, look up
       ``profiles[provider][profile_name]`` — where ``profile_name``
       comes from the backend's ``profile`` / ``runner_profile`` key,
       defaulting to ``"default"``.
    3. Otherwise, the backend has no declared capabilities and is
       treated as satisfying only an empty ``requires`` list.

    When ``requires`` is empty the input list is returned unchanged —
    the feature is opt-in and fully backward compatible.
    """
    need = {str(r).strip() for r in requires if str(r).strip()}
    if not need:
        return list(backends)

    registry = profiles or {}
    out: list[dict[str, Any]] = []
    for backend in backends:
        caps = _backend_capabilities(backend, registry)
        if caps is not None and need.issubset(caps):
            out.append(backend)
    return out


def _backend_capabilities(
    backend: dict[str, Any],
    profiles: dict[str, dict[str, ProviderProfile]],
) -> frozenset[str] | None:
    """Resolve a backend's capability set. ``None`` means unknown."""
    inline = backend.get("capabilities")
    if isinstance(inline, list):
        return frozenset(str(c).strip() for c in inline if str(c).strip())

    if backend.get("type") == "cloud":
        provider = str(backend.get("provider", "")).strip()
        profile_name = str(
            backend.get("profile") or backend.get("runner_profile") or "default"
        ).strip()
        profile = profiles.get(provider, {}).get(profile_name)
        if profile is not None:
            return profile.capabilities

    return None


def _normalize_requires(raw: Any) -> list[str]:
    if not isinstance(raw, list):
        return []
    return [str(item).strip() for item in raw if str(item).strip()]


def _profile_label(
    backend: dict[str, Any],
    profiles: dict[str, dict[str, ProviderProfile]],
) -> str:
    """Label used in the ``tried [...]`` portion of capability-miss errors."""
    if backend.get("type") == "cloud":
        provider = str(backend.get("provider", "?")).strip() or "?"
        profile_name = str(
            backend.get("profile") or backend.get("runner_profile") or "default"
        ).strip()
        return f"{provider}.{profile_name}"
    return _backend_label(backend)


def _backend_label(backend_def: dict[str, Any]) -> str:
    """Human-readable label for a backend definition."""
    btype = backend_def.get("type", "unknown")
    if btype == "vm":
        return f"vm:{backend_def.get('vm_name', '?')}"
    if btype == "cloud":
        return f"cloud:{backend_def.get('provider', '?')}"
    if btype == "ssh":
        return f"ssh:{backend_def.get('host', '?')}"
    if btype in {"ssh-windows", "ssh_windows"}:
        return f"ssh-windows:{backend_def.get('host', '?')}"
    return btype
