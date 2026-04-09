"""Local executor — runs validation on the current machine.

This is the simplest executor: shell out to the validation command
in a clean worktree, capture output, return pass/fail.

Supports two modes:
- Single command: run one shell command, check exit code
- Stage-aware: run configure/build/test as separate steps, report
  which stage failed, and enable resume from the last successful stage
"""

from __future__ import annotations

import subprocess
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.core.job import TargetResult, TargetStatus
from shipyard.core.prepared_state import (
    PreparedStateRecord,
    PreparedStateStore,
    filter_stages_by_prepared_state,
    hash_stage_commands,
)
from shipyard.executor.contract import (
    evaluate_contract,
    required_markers,
)
from shipyard.executor.streaming import ProgressCallback, run_streaming_command

STAGES = ("setup", "configure", "build", "test")


@dataclass(frozen=True)
class StageResult:
    """Result of running a single validation stage."""

    stage: str
    success: bool
    duration_secs: float
    error_message: str | None = None


class LocalExecutor:
    """Execute validation commands locally via subprocess."""

    def __init__(
        self,
        prepared_state_store: PreparedStateStore | None = None,
    ) -> None:
        # Optional store for warm-rerun stage skipping. When None,
        # prepared-state reuse is disabled regardless of project config.
        # Callers (cli.py / dispatch.py) supply this; tests construct
        # the executor without it for fresh-state semantics.
        self._prepared_state_store = prepared_state_store

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        resume_from: str | None = None,
        progress_callback: ProgressCallback | None = None,
        mode: str = "default",
    ) -> TargetResult:
        target_name = target_config.get("name", "local")
        platform = target_config.get("platform", "unknown")
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        # Validation contract — optional per project. If declared in
        # `[validation.contract]`, it gets passed to the streaming
        # layer (so markers are recorded) and to the result evaluator
        # (so missing markers can flip status to FAIL).
        contract_config = validation_config.get("contract") if validation_config else None

        # Single command mode
        if "command" in validation_config:
            return self._run_single(
                validation_config["command"], target_name, platform,
                target_config, log_file, started_at, start_time, progress_callback,
                contract_config=contract_config,
            )

        # Stage-aware mode
        stages = _get_stages(validation_config, resume_from)
        if not stages:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                error_message="No validation command configured",
                started_at=started_at, completed_at=datetime.now(timezone.utc),
            )

        return self._run_stages(
            stages, target_name, platform, target_config,
            log_file, started_at, start_time, progress_callback,
            contract_config=contract_config,
            sha=sha,
            mode=mode,
            prepared_state_enabled=_prepared_state_enabled(validation_config),
        )

    def _run_single(
        self, command: str, target_name: str, platform: str,
        target_config: dict[str, Any], log_file: Path,
        started_at: datetime, start_time: float,
        progress_callback: ProgressCallback | None,
        contract_config: dict[str, Any] | None = None,
    ) -> TargetResult:
        try:
            result = run_streaming_command(
                command,
                shell=True,
                cwd=target_config.get("cwd"),
                log_path=str(log_file),
                timeout=target_config.get("timeout_secs", 1800),
                progress_callback=progress_callback,
                required_contract_markers=required_markers(contract_config),
            )
            status = TargetStatus.PASS if result.returncode == 0 else TargetStatus.FAIL
            evaluation = evaluate_contract(contract_config, result.contract_markers_seen)
            if evaluation.should_force_fail and status == TargetStatus.PASS:
                status = TargetStatus.FAIL
            return TargetResult(
                target_name=target_name, platform=platform,
                status=status, backend="local", duration_secs=result.duration_secs,
                started_at=started_at, completed_at=result.completed_at,
                log_path=str(log_file),
                phase=result.phase,
                last_output_at=result.last_output_at,
                contract_markers_seen=evaluation.seen,
                contract_markers_missing=evaluation.missing,
                contract_violation=evaluation.message,
            )
        except subprocess.TimeoutExpired:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                duration_secs=time.monotonic() - start_time,
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message="Validation timed out",
            )
        except OSError as exc:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message=str(exc),
            )

    def _run_stages(
        self, stages: list[tuple[str, str]], target_name: str,
        platform: str, target_config: dict[str, Any], log_file: Path,
        started_at: datetime, start_time: float,
        progress_callback: ProgressCallback | None,
        contract_config: dict[str, Any] | None = None,
        sha: str = "",
        mode: str = "default",
        prepared_state_enabled: bool = False,
    ) -> TargetResult:
        """Run validation as separate stages. Stop at first failure.

        When `prepared_state_enabled` is True and a PreparedStateStore
        is wired up, stages that previously passed for the exact
        (sha, target, mode) tuple are skipped. The store is updated
        after each stage so partial progress is preserved across
        re-runs even on a failed pipeline.
        """
        failed_stage = None
        last_output_at: datetime | None = None
        # Accumulate contract markers seen across every stage. The
        # contract is evaluated once at the end of the run, so a
        # marker emitted in any stage counts.
        all_seen_markers: list[str] = []
        contract_markers_to_watch = required_markers(contract_config)

        # ── Prepared-state filtering ────────────────────────────────
        # If the project opts into prepared-state reuse and the
        # executor was constructed with a store, consult the store to
        # see whether earlier stages already passed for this exact
        # (sha, target, mode). The hash of the current stage commands
        # is part of the cache key — editing a build command in
        # config invalidates the cached state automatically.
        store = self._prepared_state_store if prepared_state_enabled else None
        config_hash = hash_stage_commands(stages)
        prepared_skipped: list[str] = []
        record: PreparedStateRecord | None = None
        if store is not None and sha:
            record = store.get(sha=sha, target=target_name, mode=mode)
            if record is not None and record.config_hash != config_hash:
                # Config changed since the last run for this SHA —
                # invalidate the cached state and start fresh.
                store.delete(sha=sha, target=target_name, mode=mode)
                record = None
            stages_to_run, prepared_skipped = filter_stages_by_prepared_state(
                stages, record, current_config_hash=config_hash,
            )
            if prepared_skipped:
                stages = stages_to_run

        try:
            log_file.write_text("")
            if prepared_skipped:
                with open(log_file, "a", encoding="utf-8") as log:
                    log.write(
                        "=== prepared-state-reuse: skipped "
                        f"{len(prepared_skipped)} stage(s) "
                        f"({', '.join(prepared_skipped)}) for sha={sha} "
                        f"target={target_name} mode={mode} ===\n"
                    )
            for stage_name, command in stages:
                stage_start = time.monotonic()
                with open(log_file, "a", encoding="utf-8") as log:
                    log.write(f"\n=== {stage_name} ===\n")
                    log.flush()

                if progress_callback:
                    progress_callback({"phase": stage_name})

                result = run_streaming_command(
                    command,
                    shell=True,
                    cwd=target_config.get("cwd"),
                    log_path=str(log_file),
                    append=True,
                    timeout=target_config.get("timeout_secs", 1800),
                    phase=stage_name,
                    progress_callback=progress_callback,
                    required_contract_markers=contract_markers_to_watch,
                )

                _ = StageResult(
                    stage=stage_name,
                    success=result.returncode == 0,
                    duration_secs=time.monotonic() - stage_start,
                )
                last_output_at = result.last_output_at
                for marker in result.contract_markers_seen:
                    if marker not in all_seen_markers:
                        all_seen_markers.append(marker)

                # Record this stage's outcome in the prepared-state
                # cache so a subsequent run on the same SHA can skip
                # everything that just passed.
                if store is not None and sha:
                    if record is None:
                        record = PreparedStateRecord(
                            sha=sha, target=target_name, mode=mode,
                            config_hash=config_hash,
                        )
                    record.mark(
                        stage_name,
                        "pass" if result.returncode == 0 else "fail",
                    )
                    store.save(record)

                if result.returncode != 0:
                    failed_stage = stage_name
                    break

        except subprocess.TimeoutExpired:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                duration_secs=time.monotonic() - start_time,
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message="Validation timed out",
            )
        except OSError as exc:
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.ERROR, backend="local",
                started_at=started_at, completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message=str(exc),
            )

        elapsed = time.monotonic() - start_time
        evaluation = evaluate_contract(contract_config, tuple(all_seen_markers))

        if failed_stage:
            error_msg = f"Stage '{failed_stage}' failed"
            return TargetResult(
                target_name=target_name, platform=platform,
                status=TargetStatus.FAIL, backend="local",
                duration_secs=elapsed, started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file), error_message=error_msg,
                phase=failed_stage,
                last_output_at=last_output_at,
                contract_markers_seen=evaluation.seen,
                contract_markers_missing=evaluation.missing,
                contract_violation=evaluation.message,
            )

        # All stages passed by exit code. Now check the contract — a
        # missing required marker can flip the status to FAIL even
        # though every stage exited 0.
        final_status = TargetStatus.PASS
        if evaluation.should_force_fail:
            final_status = TargetStatus.FAIL

        return TargetResult(
            target_name=target_name, platform=platform,
            status=final_status, backend="local",
            duration_secs=elapsed, started_at=started_at,
            completed_at=datetime.now(timezone.utc),
            log_path=str(log_file),
            phase=stages[-1][0] if stages else None,
            last_output_at=last_output_at,
            error_message=evaluation.message if final_status == TargetStatus.FAIL else None,
            contract_markers_seen=evaluation.seen,
            contract_markers_missing=evaluation.missing,
            contract_violation=evaluation.message,
        )

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Local target is always reachable."""
        return True


def _get_stages(
    validation_config: dict[str, Any], resume_from: str | None = None
) -> list[tuple[str, str]]:
    """Extract stages from config, optionally skipping to resume_from.

    When resume_from is set (e.g., "test"), earlier stages that already
    passed are skipped. This enables prepared-state resume: if the build
    succeeded but tests failed, you can re-run from "test" without
    rebuilding.
    """
    stages: list[tuple[str, str]] = []
    skipping = resume_from is not None

    for stage_name in STAGES:
        cmd = validation_config.get(stage_name)
        if not cmd:
            continue
        if skipping:
            if stage_name == resume_from:
                skipping = False
            else:
                continue
        stages.append((stage_name, cmd))

    return stages


def _prepared_state_enabled(validation_config: dict[str, Any] | None) -> bool:
    """Read the prepared-state opt-in from `[validation.prepared_state]`.

    Default: False. Projects that want warm-rerun stage skipping must
    declare it explicitly:

        [validation.prepared_state]
        enabled = true
    """
    if not validation_config:
        return False
    section = validation_config.get("prepared_state")
    if not isinstance(section, dict):
        return False
    return bool(section.get("enabled", False))
