"""SSH POSIX executor — runs validation on remote Linux/macOS hosts.

Delivers code via git bundle, then runs the validation command over SSH.
Captures output to a local log file for later inspection.
"""

from __future__ import annotations

import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.bundle.git_bundle import apply_bundle, create_bundle, upload_bundle
from shipyard.core.classify import FailureClass, classify_failure
from shipyard.core.job import TargetResult, TargetStatus
from shipyard.executor.contract import evaluate_contract, required_markers
from shipyard.executor.streaming import ProgressCallback, run_streaming_command
from shipyard.failover.retry import SSHPermanentError, SSHTransientError, is_transient, retry_ssh


class SSHExecutor:
    """Execute validation commands on a remote POSIX host via SSH."""

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        progress_callback: ProgressCallback | None = None,
        resume_from: str | None = None,
        mode: str = "default",  # accepted for API symmetry
    ) -> TargetResult:
        # `resume_from` is honored: when set, the executor probes the
        # remote for a marker file written by the previous stage's
        # successful run on this SHA. If the marker exists, earlier
        # stages are skipped on the remote. If the marker is missing
        # (or probing fails), a warning is recorded and all stages
        # run as a safe default.
        # `mode` is accepted but not yet implemented (prepared-state
        # mode tagging is local-only).
        del mode
        target_name = target_config.get("name", "ssh")
        platform = target_config.get("platform", "unknown")
        start_time = time.monotonic()
        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        @retry_ssh
        def _run() -> TargetResult:
            result = self._validate_once(
                sha=sha,
                branch=branch,
                target_config=target_config,
                validation_config=validation_config,
                log_path=log_path,
                progress_callback=progress_callback,
                resume_from=resume_from,
            )
            if result.status == TargetStatus.ERROR and result.error_message and is_transient(result.error_message):
                raise RuntimeError(result.error_message)
            return result

        try:
            return _run()
        except (SSHTransientError, SSHPermanentError) as exc:
            return _error_result(
                target_name,
                platform,
                datetime.now(timezone.utc),
                start_time,
                str(log_file),
                str(exc),
            )

    def _validate_once(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
        progress_callback: ProgressCallback | None = None,
        resume_from: str | None = None,
    ) -> TargetResult:
        target_name = target_config.get("name", "ssh")
        platform = target_config.get("platform", "unknown")
        host = target_config["host"]
        remote_repo = target_config.get("repo_path", "~/repo")
        ssh_options = _ssh_options(target_config)
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        # Step 1: Deliver code to the remote host.
        # First check if the remote already has the SHA (from a prior
        # run or a `git fetch origin`). If it does, skip the expensive
        # bundle create → scp upload → apply pipeline entirely. For
        # Pulp's 370 MB repo the bundle is ~443 MB; skipping it saves
        # 10–30 minutes on Windows and ~30 seconds on Linux.
        if _remote_has_sha(host, remote_repo, sha, ssh_options):
            pass  # remote is ready, skip bundle delivery
        else:
            with tempfile.TemporaryDirectory() as tmpdir:
                bundle_path = Path(tmpdir) / "shipyard.bundle"
                remote_bundle = target_config.get(
                    "remote_bundle_path", "/tmp/shipyard.bundle"
                )
                local_repo_dir = target_config.get("local_repo_dir")

                # Try an incremental bundle first: query the remote
                # for its HEAD SHA and use it as a basis so only the
                # delta is bundled. The remote HEAD must also exist
                # as an ancestor in the local repo, otherwise
                # `git bundle create ^<basis>` has no meaningful cut
                # point and either fails or silently produces a full
                # bundle. Falls back to a full bundle if the remote
                # HEAD is unknown, not locally reachable, or the
                # incremental bundle fails.
                basis_shas: list[str] = []
                bundle_mode = "full"
                remote_head = _remote_head_sha(host, remote_repo, ssh_options)
                if remote_head and _local_has_commit(
                    remote_head, repo_dir=local_repo_dir,
                ):
                    basis_shas.append(remote_head)

                bundle_result = create_bundle(
                    sha=sha,
                    output_path=bundle_path,
                    repo_dir=local_repo_dir,
                    basis_shas=basis_shas,
                )
                if bundle_result.success and basis_shas:
                    bundle_mode = "delta"

                # If incremental bundle failed and we had a basis,
                # fall back to a full bundle.
                if not bundle_result.success and basis_shas:
                    bundle_result = create_bundle(
                        sha=sha,
                        output_path=bundle_path,
                        repo_dir=local_repo_dir,
                    )
                    bundle_mode = "full"
                if not bundle_result.success:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file), f"Bundle creation failed: {bundle_result.message}",
                    )

                bundle_bytes = _safe_filesize(bundle_path)
                _append_log(
                    log_file,
                    f"=== bundle_mode={bundle_mode} bundle_bytes={bundle_bytes} "
                    f"sha={sha} remote_head={remote_head or 'unknown'} ===\n",
                )

                upload_result = upload_bundle(
                    bundle_path=bundle_path,
                    host=host,
                    remote_path=remote_bundle,
                    ssh_options=ssh_options,
                    timeout=int(target_config.get("bundle_upload_timeout_secs", 1800)),
                )
                if not upload_result.success:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file), f"Bundle upload failed: {upload_result.message}",
                    )

                apply_result = apply_bundle(
                    host=host,
                    bundle_path=remote_bundle,
                    repo_path=remote_repo,
                    ssh_options=ssh_options,
                    timeout=int(target_config.get("bundle_apply_timeout_secs", 1800)),
                )
                if not apply_result.success:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file), f"Bundle apply failed: {apply_result.message}",
                    )

        # Step 2: Resolve resume_from. If the caller asked to skip
        # earlier stages, probe the remote for a marker file written
        # by the previous successful run on this SHA. If the marker
        # is missing, run all stages (safe default) and log a note.
        effective_resume = _resolve_resume_from(
            host=host,
            remote_repo=remote_repo,
            sha=sha,
            ssh_options=ssh_options,
            requested=resume_from,
            validation_config=validation_config,
            log_file=log_file,
        )

        # Step 3: Checkout the SHA and run validation
        command = _build_remote_command(
            sha, remote_repo, validation_config, resume_from=effective_resume,
        )
        if not command:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), "No validation command configured",
            )

        ssh_cmd = ["ssh"] + list(ssh_options) + [host, command]

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

            failure_class: str | None = None
            if status != TargetStatus.PASS:
                failure_class = classify_failure(
                    stdout="",
                    stderr=result.output or error_message or "",
                    exit_code=result.returncode,
                    contract_violated=evaluation.violated and evaluation.enforce,
                ).value

            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=status,
                backend="ssh",
                duration_secs=result.duration_secs,
                started_at=started_at,
                completed_at=result.completed_at,
                log_path=str(log_file),
                phase=result.phase,
                last_output_at=result.last_output_at,
                last_heartbeat_at=result.last_heartbeat_at,
                error_message=error_message,
                contract_markers_seen=evaluation.seen,
                contract_markers_missing=evaluation.missing,
                contract_violation=evaluation.message,
                failure_class=failure_class,
            )

        except subprocess.TimeoutExpired:
            elapsed = time.monotonic() - start_time
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="ssh",
                duration_secs=elapsed,
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
                error_message="Validation timed out",
                failure_class=FailureClass.TIMEOUT.value,
            )

        except OSError as exc:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), str(exc),
            )

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Check SSH reachability with a quick echo command."""
        diag = self._probe_ssh(target_config)
        return diag["reachable"]

    def diagnose(self, target_config: dict[str, Any]) -> dict[str, Any]:
        """Return a rich reachability diagnosis for preflight.

        The preflight surfaces `message` + `category` in the
        user-facing error on fail-fast. Categories are stable strings
        callers can branch on: "auth", "host_key", "network",
        "timeout", "unknown".
        """
        diag = self._probe_ssh(target_config)
        return {
            "reachable": diag["reachable"],
            "message": _format_ssh_diagnosis(target_config, diag),
            "category": diag["category"],
        }

    def _probe_ssh(self, target_config: dict[str, Any]) -> dict[str, Any]:
        host = target_config.get("host")
        if not host:
            return {
                "reachable": False,
                "category": "configuration",
                "last_error": "target has no host configured",
                "timed_out": False,
            }

        ssh_options = _ssh_options(target_config)
        cmd = (
            ["ssh"]
            + list(ssh_options)
            + [
                "-o", "ConnectTimeout=5",
                "-o", "BatchMode=yes",
                host,
                "echo ok",
            ]
        )

        try:
            result = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=10,
            )
        except subprocess.TimeoutExpired:
            return {
                "reachable": False,
                "category": "timeout",
                "last_error": "ssh probe exceeded 10s",
                "timed_out": True,
            }
        except OSError as exc:
            return {
                "reachable": False,
                "category": "network",
                "last_error": str(exc),
                "timed_out": False,
            }

        if result.returncode == 0:
            return {
                "reachable": True,
                "category": None,
                "last_error": None,
                "timed_out": False,
            }

        stderr = (result.stderr or "").strip()
        return {
            "reachable": False,
            "category": _classify_probe_error(stderr, result.returncode),
            "last_error": stderr or f"ssh exited {result.returncode}",
            "timed_out": False,
        }


def _remote_head_sha(
    host: str,
    repo_path: str,
    ssh_options: list[str],
    *,
    timeout: int = 15,
) -> str | None:
    """Return the HEAD SHA from the remote repo, or None on any error.

    Used to create incremental bundles: if the remote is at commit X
    and we need to deliver commit Y, the bundle only needs objects
    reachable from Y but not from X. For a typical 1-2 commit delta
    this reduces a 443 MB bundle to a few KB.

    Returns None on any error (SSH unreachable, empty repo, timeout)
    so the caller falls through to a full bundle as a safe default.
    """
    cmd = [
        "ssh",
        *ssh_options,
        "-o", "ConnectTimeout=5",
        host,
        f"cd {repo_path} && git rev-parse HEAD",
    ]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
        if result.returncode == 0:
            sha = result.stdout.strip()
            # Sanity-check: must look like a hex SHA
            if sha and len(sha) >= 7 and all(c in "0123456789abcdef" for c in sha):
                return sha
        return None
    except (subprocess.SubprocessError, OSError):
        return None


def _local_has_commit(
    sha: str,
    repo_dir: str | None = None,
    *,
    timeout: int = 10,
) -> bool:
    """Return True when the local repo has `sha` as a reachable commit.

    Used to validate a candidate basis SHA for incremental-bundle
    creation. `git bundle create <target> ^<basis>` only produces a
    meaningful delta when `<basis>` is an ancestor of `<target>` in
    the local object store; if the local clone hasn't fetched the
    remote's HEAD yet (e.g. the remote was updated out-of-band, or
    the basis was rewritten), the negation is a no-op and we'd waste
    bandwidth shipping what is effectively a full bundle.

    Returns False on any error (missing git, bad cwd, timeout) so
    the caller falls back to a full bundle as a safe default.
    """
    cmd = ["git", "cat-file", "-e", f"{sha}^{{commit}}"]
    try:
        result = subprocess.run(
            cmd,
            cwd=repo_dir,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return result.returncode == 0
    except (subprocess.SubprocessError, OSError):
        return False


def _safe_filesize(path: Path) -> int:
    """Return file size in bytes, or -1 if the file can't be stat'd."""
    try:
        return path.stat().st_size
    except OSError:
        return -1


def _remote_has_sha(
    host: str,
    repo_path: str,
    sha: str,
    ssh_options: list[str],
    *,
    timeout: int = 15,
) -> bool:
    """Check whether the remote repo already contains the given SHA.

    Runs `git cat-file -e <sha>` on the remote via SSH. If the object
    exists, the bundle create → scp upload → apply pipeline can be
    skipped entirely — saving 10–30 minutes on large repos where the
    full bundle is hundreds of megabytes. This is the most impactful
    single optimisation for repeat runs against the same host: the
    first run still needs the bundle, but every subsequent run on the
    same or nearby SHA is effectively free.

    Returns False on any error (SSH unreachable, timeout, bad path) so
    the caller falls through to the bundle path as a safe default.
    """
    cmd = [
        "ssh",
        *ssh_options,
        "-o", "ConnectTimeout=5",
        host,
        f"cd {repo_path} && git cat-file -e {sha}",
    ]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
        return result.returncode == 0
    except (subprocess.SubprocessError, OSError):
        return False


def _ssh_options(target_config: dict[str, Any]) -> list[str]:
    """Extract SSH options from target config."""
    options: list[str] = []
    if "ssh_options" in target_config:
        options.extend(target_config["ssh_options"])
    if "identity_file" in target_config:
        options.extend(["-i", target_config["identity_file"]])
    return options


STAGE_ORDER = ("setup", "configure", "build", "test")


def _stage_marker_name(stage: str, sha: str) -> str:
    """Marker file written to the remote repo after a stage succeeds.

    SHA-scoped so artifacts from a different commit can't cause a
    false skip on a later resume. We use a short SHA prefix to keep
    the filename readable; collisions on a repo with many recent
    runs would only cause a stale skip, which is bounded by the
    ``--resume-from`` opt-in (the user is asserting they want to
    skip).
    """
    return f".shipyard-stage-{stage}-{sha[:12]}"


def _resolve_resume_from(
    *,
    host: str,
    remote_repo: str,
    sha: str,
    ssh_options: list[str],
    requested: str | None,
    validation_config: dict[str, Any],
    log_file: Path,
) -> str | None:
    """Decide whether to honor a `--resume-from` request.

    The request is honored only when the previous stage's marker
    file exists on the remote for this exact SHA. Otherwise the
    function logs a note and returns None so all stages run.

    Single-command validation configs (no stage breakdown) cannot
    resume — the request is logged and ignored.
    """
    if requested is None:
        return None
    if "command" in validation_config:
        _append_log(
            log_file,
            f"=== resume-from: ignored ({requested!r}) — "
            f"validation_config uses single command, no stages to skip ===\n",
        )
        return None
    if requested not in STAGE_ORDER:
        _append_log(
            log_file,
            f"=== resume-from: ignored — unknown stage {requested!r} ===\n",
        )
        return None

    idx = STAGE_ORDER.index(requested)
    if idx == 0:
        # Resuming from the first stage is a no-op
        return requested

    # Look for the most recent stage with a configured command
    # before requested; that's the marker we need to find.
    prev_stage = None
    for candidate in STAGE_ORDER[:idx][::-1]:
        if validation_config.get(candidate):
            prev_stage = candidate
            break

    if prev_stage is None:
        return requested  # no earlier stages configured anyway

    marker = _stage_marker_name(prev_stage, sha)
    if _remote_marker_exists(host, remote_repo, marker, ssh_options):
        _append_log(
            log_file,
            f"=== resume-from: honoring {requested!r} — found marker for "
            f"previous stage {prev_stage!r} on remote ===\n",
        )
        return requested
    _append_log(
        log_file,
        f"=== resume-from: requested {requested!r} but marker for "
        f"previous stage {prev_stage!r} not found on remote — running "
        f"all stages from the beginning ===\n",
    )
    return None


def _remote_marker_exists(
    host: str,
    repo_path: str,
    marker: str,
    ssh_options: list[str],
    *,
    timeout: int = 15,
) -> bool:
    """Check whether `<repo>/<marker>` exists on the remote."""
    cmd = [
        "ssh",
        *ssh_options,
        "-o", "ConnectTimeout=5",
        host,
        f"test -f {shlex_quote(repo_path)}/{shlex_quote(marker)}",
    ]
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
        return result.returncode == 0
    except (subprocess.SubprocessError, OSError):
        return False


def _append_log(log_file: Path, text: str) -> None:
    try:
        with open(log_file, "a", encoding="utf-8") as fh:
            fh.write(text)
    except OSError:
        pass


def shlex_quote(value: str) -> str:
    """Local re-export so callers don't need to import shlex separately."""
    import shlex
    return shlex.quote(value)


def _build_remote_command(
    sha: str,
    remote_repo: str,
    validation_config: dict[str, Any],
    resume_from: str | None = None,
) -> str | None:
    """Build the remote shell command: checkout + validate.

    When ``resume_from`` is set, stages before the resume point are
    skipped. After each stage succeeds, a marker file is written to
    the repo root so a subsequent resume run can detect that the
    earlier stage really did pass on this SHA.
    """
    import shlex

    if "command" in validation_config:
        validate_cmd = validation_config["command"]
    else:
        parts: list[str] = []
        skipping = resume_from is not None
        for step in STAGE_ORDER:
            cmd = validation_config.get(step)
            if not cmd:
                continue
            if skipping:
                if step == resume_from:
                    skipping = False
                else:
                    continue
            marker = _stage_marker_name(step, sha)
            parts.append(
                f"printf '__SHIPYARD_PHASE__:{step}\\n' && "
                f"{cmd} && touch {shlex.quote(marker)}"
            )
        if not parts:
            return None
        validate_cmd = " && ".join(parts)

    # Quote the repo path in case it contains spaces or shell
    # metacharacters. sha is validated upstream to be a git hash so
    # it's shell-safe, but we quote it anyway for consistency.
    return (
        f"cd {shlex.quote(remote_repo)} && "
        f"git checkout --force {shlex.quote(sha)} && "
        f"{validate_cmd}"
    )


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
        backend="ssh",
        duration_secs=time.monotonic() - start_time,
        started_at=started_at,
        completed_at=datetime.now(timezone.utc),
        log_path=log_path,
        error_message=message,
        failure_class=_classify_ssh_error(message),
    )


def _classify_ssh_error(message: str) -> str:
    """Classify an SSH-level error for ``_error_result`` callers.

    Called only after we've already decided the outcome is ERROR, so
    the classifier never returns CONTRACT / TEST / TIMEOUT here — the
    fingerprints in the error message pick INFRA or UNKNOWN.
    """
    return classify_failure(
        stdout="", stderr=message, exit_code=-1,
    ).value


def _extract_ssh_error(output: str) -> str | None:
    for line in reversed(output.splitlines()):
        if line.strip():
            return line.strip()
    return None


def _classify_probe_error(stderr: str, returncode: int) -> str:
    """Best-effort classification of an ssh probe failure.

    Stable string buckets — preflight surfaces these back to the user
    and tooling can branch on them without pattern-matching the raw
    ssh error text.
    """
    lowered = stderr.lower()
    if (
        "permission denied" in lowered
        or "publickey" in lowered
        or "too many authentication failures" in lowered
    ):
        return "auth"
    if (
        "host key verification failed" in lowered
        or "remote host identification has changed" in lowered
        or "offending" in lowered
    ):
        return "host_key"
    if (
        "could not resolve hostname" in lowered
        or "name or service not known" in lowered
        or "nodename nor servname" in lowered  # macOS variant
        or "no route to host" in lowered
        or "network is unreachable" in lowered
        or "connection refused" in lowered
    ):
        return "network"
    if "connection timed out" in lowered:
        return "timeout"
    if returncode == 255:
        return "network"
    return "unknown"


def _format_ssh_diagnosis(
    target_config: dict[str, Any], diag: dict[str, Any]
) -> str:
    """Compose the preflight error message for an unreachable SSH target."""
    host = target_config.get("host") or "<no host>"
    user_at_host = host if "@" in host else host
    port = target_config.get("port")
    if port:
        user_at_host = f"{user_at_host}:{port}"
    lines = [
        f"SSH backend unreachable at {user_at_host}.",
        f"  Attempted: 10s probe with ConnectTimeout=5, BatchMode=yes.",
    ]
    category = diag.get("category")
    if category:
        lines.append(f"  Failure category: {category}")
    last = diag.get("last_error")
    if last:
        lines.append(f"  Last error: {last}")
    return "\n".join(lines)
