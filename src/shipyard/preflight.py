"""Submission preflight checks for CLI-triggered runs."""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

from shipyard.failover.auto import apply_auto_cloud_fallback

if TYPE_CHECKING:
    from collections.abc import Iterable

    from shipyard.core.config import Config
    from shipyard.executor.dispatch import ExecutorDispatcher


# Exit codes surfaced by the CLI. ValueError → 1 (validation/config); we
# want a distinct exit for "backend physically can't be reached" so
# automation can branch without parsing error strings.
EXIT_BACKEND_UNREACHABLE = 3


class BackendUnreachableError(RuntimeError):
    """At least one target's primary + fallback backends are unreachable.

    Deliberately not a ValueError subclass — the CLI distinguishes
    between configuration problems (exit 2) and hitting a real dead
    backend (exit 3) so cron/agent automation can react without
    parsing error strings.
    """


@dataclass(frozen=True)
class TargetPreflight:
    """Preflight result for a single target."""

    target_name: str
    backend: str
    reachable: bool
    selected_backend: str
    message: str | None = None
    failure_category: str | None = None

    def to_dict(self) -> dict[str, Any]:
        data = {
            "target": self.target_name,
            "backend": self.backend,
            "reachable": self.reachable,
            "selected_backend": self.selected_backend,
        }
        if self.message:
            data["message"] = self.message
        if self.failure_category:
            data["failure_category"] = self.failure_category
        return data


@dataclass(frozen=True)
class PreflightResult:
    """Aggregate submission preflight result."""

    git_root: Path | None
    expected_root: Path | None
    targets: dict[str, TargetPreflight] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)
    skipped_targets: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "git_root": str(self.git_root) if self.git_root else None,
            "expected_root": str(self.expected_root) if self.expected_root else None,
            "targets": {name: state.to_dict() for name, state in self.targets.items()},
            "warnings": list(self.warnings),
            "skipped_targets": list(self.skipped_targets),
        }


def run_submission_preflight(
    config: Config,
    *,
    target_names: list[str],
    dispatcher: ExecutorDispatcher,
    allow_root_mismatch: bool = False,
    allow_unreachable_targets: bool = False,
    skip_targets: Iterable[str] = (),
    cwd: Path | None = None,
) -> PreflightResult:
    """Check repo root and target reachability before enqueueing a job.

    `skip_targets` deliberately removes a lane before the probe runs —
    the caller is asserting "I don't want to validate this target at
    all" (semantics distinct from `allow_unreachable_targets`, which
    means "I'll accept a validation gap if this lane can't be reached
    right now").
    """
    workdir = (cwd or Path.cwd()).resolve()
    expected_root = config.project_dir.parent.resolve() if config.project_dir else workdir
    git_root = _git_root_for(workdir)

    warnings: list[str] = []
    if git_root and git_root != expected_root:
        message = f"Git root {git_root} does not match Shipyard project root {expected_root}"
        if allow_root_mismatch:
            warnings.append(message)
        else:
            raise ValueError(message)

    # Apply --skip-target BEFORE the probe so a deliberate skip never
    # wastes a 10-second ssh handshake. This is the semantics
    # difference vs --allow-unreachable-targets.
    skip_set = {name for name in skip_targets}
    skipped_present = [n for n in target_names if n in skip_set]
    skipped_unknown = sorted(skip_set - set(target_names))
    if skipped_unknown:
        raise ValueError(
            "skip-target names no configured target: "
            + ", ".join(skipped_unknown)
        )
    for name in skipped_present:
        warnings.append(
            f"Target '{name}' deliberately skipped (--skip-target)."
        )
    effective_targets = [n for n in target_names if n not in skip_set]

    # Inject an implicit cloud fallback on SSH targets when the
    # project opts into [failover.cloud_auto]. Targets that already
    # declare their own `fallback = [...]` are left alone. This
    # matches Pulp's local_ci.py namespace_auto behavior — a solo
    # developer can leave their Windows VM shut down and Shipyard
    # will route the run through the cloud instead of erroring.
    injected = apply_auto_cloud_fallback(config)
    if injected:
        warnings.append(
            "auto-cloud-failover injected for: " + ", ".join(injected),
        )

    target_states: dict[str, TargetPreflight] = {}
    unreachable: list[TargetPreflight] = []
    for target_name in effective_targets:
        target_config = dict(config.targets.get(target_name, {}))
        target_config["name"] = target_name
        primary_backend = dispatcher.backend_name(target_config)

        probe_result = _probe_target_path(target_config, dispatcher)
        if not probe_result.reachable:
            unreachable.append(probe_result)

        if probe_result.selected_backend != primary_backend:
            warnings.append(
                f"Target '{target_name}' primary backend '{primary_backend}' is unavailable; "
                f"preflight selected failover backend '{probe_result.selected_backend}'."
            )

        target_states[target_name] = probe_result

    if unreachable:
        if allow_unreachable_targets:
            # Loud warning: the user asked for this escape hatch, but
            # they probably didn't realize they were buying a
            # validation gap. Surfacing it prominently gives agents
            # a fighting chance of not "just adding --allow-…" as a
            # muscle-memory workaround for a real backend outage.
            names = ", ".join(u.target_name for u in unreachable)
            warnings.append(
                f"⚠︎ VALIDATION GAP — the following targets are unreachable "
                f"and will be SKIPPED, NOT validated: {names}. "
                f"Each lane's evidence will be absent from the merge "
                f"gate. Fix the backend, or use --skip-target <name> to "
                f"record the skip deliberately."
            )
            for u in unreachable:
                detail = u.message or f"Target '{u.target_name}' is unreachable"
                warnings.append(f"  - {detail}")
        else:
            raise BackendUnreachableError(
                _format_unreachable_error(unreachable)
            )

    return PreflightResult(
        git_root=git_root,
        expected_root=expected_root,
        targets=target_states,
        warnings=warnings,
        skipped_targets=skipped_present,
    )


def _format_unreachable_error(unreachable: list[TargetPreflight]) -> str:
    """Compose the multi-line error that fires on fail-fast."""
    lines: list[str] = []
    for u in unreachable:
        header = f"Target '{u.target_name}' ({u.backend}) is unreachable."
        lines.append(header)
        if u.message:
            for detail_line in u.message.splitlines():
                lines.append(f"  {detail_line}")
        if u.failure_category:
            lines.append(f"  category: {u.failure_category}")

    lines.append("")
    lines.append("Options:")
    lines.append(
        "  - Fix the backend (check network, SSH key, hostname in ~/.ssh/config)"
    )
    lines.append(
        "  - Re-run with --skip-target <name> to deliberately skip this lane"
    )
    lines.append(
        "  - Re-run with --allow-unreachable-targets to proceed "
        "(LANE WILL BE SKIPPED, NOT VALIDATED)"
    )
    return "\n".join(lines)


def _probe_target_path(
    target_config: dict[str, Any],
    dispatcher: ExecutorDispatcher,
) -> TargetPreflight:
    target_name = target_config.get("name", "unknown")
    primary_backend = dispatcher.backend_name(target_config)

    if dispatcher.probe(target_config):
        return TargetPreflight(
            target_name=target_name,
            backend=primary_backend,
            reachable=True,
            selected_backend=primary_backend,
        )

    for fallback in target_config.get("fallback", []):
        merged_config = {**target_config, **fallback}
        fallback_backend = dispatcher.backend_name(merged_config)
        if dispatcher.probe(merged_config):
            return TargetPreflight(
                target_name=target_name,
                backend=primary_backend,
                reachable=True,
                selected_backend=fallback_backend,
                message=f"Primary backend '{primary_backend}' unreachable; failover '{fallback_backend}' is available",
            )

    # Neither primary nor any fallback is reachable — ask the
    # dispatcher for a richer diagnosis if it offers one, so the
    # user sees "auth refused" instead of a generic "unreachable".
    diagnosis = _collect_diagnosis(target_config, dispatcher)
    return TargetPreflight(
        target_name=target_name,
        backend=primary_backend,
        reachable=False,
        selected_backend=primary_backend,
        message=diagnosis.message,
        failure_category=diagnosis.category,
    )


@dataclass(frozen=True)
class _ProbeDiagnosis:
    message: str
    category: str | None


def _collect_diagnosis(
    target_config: dict[str, Any],
    dispatcher: ExecutorDispatcher,
) -> _ProbeDiagnosis:
    """Best-effort richer error for the unreachable case."""
    diagnose = getattr(dispatcher, "diagnose", None)
    if callable(diagnose):
        try:
            result = diagnose(target_config)
        except Exception:
            result = None
        if result is not None:
            return _ProbeDiagnosis(
                message=result.get("message")
                or f"Target '{target_config.get('name','unknown')}' has no reachable backend",
                category=result.get("category"),
            )
    return _ProbeDiagnosis(
        message=f"Target '{target_config.get('name','unknown')}' has no reachable backend",
        category=None,
    )


def _git_root_for(path: Path) -> Path | None:
    try:
        output = subprocess.check_output(
            ["git", "rev-parse", "--show-toplevel"],
            cwd=path,
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except (subprocess.CalledProcessError, FileNotFoundError):
        return None
    return Path(output).resolve()
