"""Submission preflight checks for CLI-triggered runs."""

from __future__ import annotations

import subprocess
from dataclasses import dataclass, field
from pathlib import Path
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from shipyard.core.config import Config
    from shipyard.executor.dispatch import ExecutorDispatcher


@dataclass(frozen=True)
class TargetPreflight:
    """Preflight result for a single target."""

    target_name: str
    backend: str
    reachable: bool
    selected_backend: str
    message: str | None = None

    def to_dict(self) -> dict[str, Any]:
        data = {
            "target": self.target_name,
            "backend": self.backend,
            "reachable": self.reachable,
            "selected_backend": self.selected_backend,
        }
        if self.message:
            data["message"] = self.message
        return data


@dataclass(frozen=True)
class PreflightResult:
    """Aggregate submission preflight result."""

    git_root: Path | None
    expected_root: Path | None
    targets: dict[str, TargetPreflight] = field(default_factory=dict)
    warnings: list[str] = field(default_factory=list)

    def to_dict(self) -> dict[str, Any]:
        return {
            "git_root": str(self.git_root) if self.git_root else None,
            "expected_root": str(self.expected_root) if self.expected_root else None,
            "targets": {name: state.to_dict() for name, state in self.targets.items()},
            "warnings": list(self.warnings),
        }


def run_submission_preflight(
    config: Config,
    *,
    target_names: list[str],
    dispatcher: ExecutorDispatcher,
    allow_root_mismatch: bool = False,
    allow_unreachable_targets: bool = False,
    cwd: Path | None = None,
) -> PreflightResult:
    """Check repo root and target reachability before enqueueing a job."""
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

    target_states: dict[str, TargetPreflight] = {}
    for target_name in target_names:
        target_config = dict(config.targets.get(target_name, {}))
        target_config["name"] = target_name
        primary_backend = dispatcher.backend_name(target_config)

        probe_result = _probe_target_path(target_config, dispatcher)
        if not probe_result.reachable:
            message = probe_result.message or f"Target '{target_name}' is unreachable"
            if allow_unreachable_targets:
                warnings.append(message)
            else:
                raise ValueError(message)

        if probe_result.selected_backend != primary_backend:
            warnings.append(
                f"Target '{target_name}' primary backend '{primary_backend}' is unavailable; "
                f"preflight selected failover backend '{probe_result.selected_backend}'."
            )

        target_states[target_name] = probe_result

    return PreflightResult(
        git_root=git_root,
        expected_root=expected_root,
        targets=target_states,
        warnings=warnings,
    )


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

    return TargetPreflight(
        target_name=target_name,
        backend=primary_backend,
        reachable=False,
        selected_backend=primary_backend,
        message=f"Target '{target_name}' has no reachable backend",
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
