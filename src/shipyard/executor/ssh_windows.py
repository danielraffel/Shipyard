"""SSH Windows executor — runs validation on remote Windows hosts via PowerShell.

Same bundle-based delivery as the POSIX SSH executor, but uses PowerShell
semantics for remote commands and Windows-style paths.

Multi-line PowerShell scripts are always sent via ``-EncodedCommand`` with
a base64-encoded UTF-16LE payload rather than ``-Command <raw script>``.
The naive ``-Command`` path silently drops every line after the first when
the script reaches PowerShell through Windows OpenSSH's cmd.exe default
shell — each newline is interpreted by cmd.exe as a command separator.
A 60-line mutex wrapper would run only its first line (a silent
``$ErrorActionPreference = 'Stop'`` assignment), exit 0, and return an
empty stdout buffer. Shipyard then reported the target as ``pass`` in
0.9 seconds with an empty log: a silent false-green. ``-EncodedCommand``
bypasses cmd.exe entirely because the payload is a single argv token.
"""

from __future__ import annotations

import base64
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

from shipyard.bundle.git_bundle import create_bundle, upload_bundle
from shipyard.core.job import TargetResult, TargetStatus
from shipyard.executor.clixml import maybe_decode_clixml
from shipyard.executor.contract import evaluate_contract, required_markers
from shipyard.executor.streaming import ProgressCallback, run_streaming_command
from shipyard.executor.windows_toolchain import (
    DEFAULT_MUTEX_NAME,
    VsToolchain,
    detect_vs_toolchain,
    toolchain_env_exports,
    wrap_powershell_with_host_mutex,
)


class SSHWindowsExecutor:
    """Execute validation commands on a remote Windows host via SSH + PowerShell."""

    def __init__(self) -> None:
        # Cache of detected VS toolchains keyed by (host, ssh_options
        # tuple). Detection shells out to vswhere, which is slow
        # enough (~1s) that we want to reuse it within a single
        # Shipyard invocation — but two targets with the same
        # hostname and different ssh options (e.g. different users,
        # ports, or bastion jump hosts) can land on completely
        # different machines, so the cache key must include the
        # full connection identity, not just the host string.
        self._vs_toolchain_cache: dict[
            tuple[str, tuple[str, ...]], VsToolchain | None
        ] = {}

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
        # stages are skipped on the remote.
        # `mode` is accepted but not yet implemented (prepared-state
        # mode tagging is local-only).
        del mode
        target_name = target_config.get("name", "windows")
        platform = target_config.get("platform", "windows-x64")
        host = target_config.get("host")
        if not host:
            # #120: never let a missing `host` crash with KeyError.
            # Surface a clean ERROR result so the ship flow exits with
            # a real message instead of a traceback.
            now = datetime.now(timezone.utc)
            log_file = Path(log_path)
            log_file.parent.mkdir(parents=True, exist_ok=True)
            return _error_result(
                target_name, platform, now, time.monotonic(),
                str(log_file),
                f"Target '{target_name}' is misconfigured: no `host` field in "
                f".shipyard/config.toml or .shipyard.local/config.toml.",
            )
        remote_repo = target_config.get("repo_path", "C:\\repo")
        ssh_options = _ssh_options(target_config)
        started_at = datetime.now(timezone.utc)
        start_time = time.monotonic()

        log_file = Path(log_path)
        log_file.parent.mkdir(parents=True, exist_ok=True)

        # Step 1: Deliver code to the remote host.
        # First check if the remote already has the SHA (from a prior
        # run or a `git fetch origin`). If it does, skip the expensive
        # bundle create → scp upload → apply pipeline entirely. For
        # Pulp's 370 MB repo the bundle is ~443 MB and scp to Windows
        # takes 10–30 min with a recurring SFTP-close hang; skipping
        # the bundle when possible is the single highest-impact
        # optimisation for Windows iteration speed.
        if _remote_has_sha_windows(host, remote_repo, sha, ssh_options):
            pass  # remote is ready, skip bundle delivery
        else:
            with tempfile.TemporaryDirectory() as tmpdir:
                bundle_path = Path(tmpdir) / "shipyard.bundle"
                # Use a home-relative path by default rather than
                # `C:\Temp\shipyard.bundle`, which doesn't exist on a
                # stock Windows install and caused
                # `scp: dest open "C:\\Temp\\shipyard.bundle": No such
                # file or directory` on the first Stage 1 Windows
                # dogfood run. A bare filename lands in the SSH user's
                # home directory, which always exists.
                remote_bundle = target_config.get(
                    "remote_bundle_path", "shipyard.bundle"
                )

                # Try an incremental bundle first: query the remote
                # for its HEAD SHA and use it as a basis so only the
                # delta is bundled. Falls back to a full bundle if
                # the remote HEAD is unknown or the incremental
                # bundle fails (e.g. no common ancestor).
                basis_shas: list[str] = []
                remote_head = _remote_head_sha_windows(host, remote_repo, sha, ssh_options)
                if remote_head:
                    basis_shas.append(remote_head)

                bundle_result = create_bundle(
                    sha=sha,
                    output_path=bundle_path,
                    repo_dir=target_config.get("local_repo_dir"),
                    basis_shas=basis_shas,
                )

                # If incremental bundle failed and we had a basis,
                # fall back to a full bundle.
                if not bundle_result.success and basis_shas:
                    bundle_result = create_bundle(
                        sha=sha,
                        output_path=bundle_path,
                        repo_dir=target_config.get("local_repo_dir"),
                    )
                if not bundle_result.success:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file),
                        f"Bundle creation failed: {bundle_result.message}",
                    )

                # #239 Phase A: write an initial log header before
                # upload starts so the per-target log file exists
                # even if upload fails pre-stage. The user's Pulp
                # repro had the ship-flow print a "see log at
                # windows.log" hint but the file didn't exist —
                # because error exits happen before any logger
                # touches it. Now the file always exists with the
                # attempted command + target metadata, and
                # per-attempt upload stderr appended on failure.
                try:
                    with log_file.open("w", encoding="utf-8") as lf:
                        lf.write(
                            f"# shipyard ssh-windows lane log\n"
                            f"target: {target_name}\n"
                            f"platform: {platform}\n"
                            f"host: {host}\n"
                            f"sha: {sha}\n"
                            f"branch: {branch}\n"
                            f"remote_repo: {remote_repo}\n"
                            f"remote_bundle: {remote_bundle}\n"
                            f"started_at: {started_at.isoformat()}\n"
                            f"---\n"
                        )
                except OSError:
                    # Don't let logging failure cascade; the ship
                    # flow's "see log" hint will point at a missing
                    # file and that's fine. Upload proceeds.
                    pass

                upload_result = upload_bundle(
                    bundle_path=bundle_path,
                    host=host,
                    remote_path=remote_bundle,
                    ssh_options=ssh_options,
                    timeout=int(target_config.get("bundle_upload_timeout_secs", 1800)),
                    is_windows=True,
                )
                if not upload_result.success:
                    # #239 Phase A: append per-attempt upload stderr
                    # to the log so the operator has concrete data
                    # on whether this was a connect-timeout, a slow
                    # runner, or mid-stream upload break. Without
                    # this the only artifact was the summary line
                    # "Upload failed: ssh: connect to host ...".
                    try:
                        with log_file.open("a", encoding="utf-8") as lf:
                            lf.write(
                                f"bundle-upload failure "
                                f"(class={upload_result.failure_class})\n"
                            )
                            for attempt_line in upload_result.attempts:
                                lf.write(f"  {attempt_line}\n")
                            lf.write(f"summary: {upload_result.message}\n")
                    except OSError:
                        pass
                    # Surface the classification in the user-facing
                    # error message so the summary row tells the
                    # user "ssh-unreachable" vs "upload failed
                    # after reachable" without them opening the
                    # log.
                    class_hint = ""
                    if upload_result.failure_class == "ssh_unreachable":
                        class_hint = " [ssh-unreachable]"
                    elif upload_result.failure_class == "upload_failed":
                        class_hint = " [upload failed after reachable]"
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file),
                        f"Bundle upload failed{class_hint}: "
                        f"{upload_result.message}",
                    )

                # #247: verify the upload actually landed before
                # handing off to git. upload_bundle's success can
                # be reported even when scp closes cleanly with zero
                # bytes actually on disk — in that failure mode git
                # emits "error: could not open '...'" which PowerShell
                # wraps in a CLIXML envelope + progress records, and
                # the operator sees pulp#728-shaped garbage instead
                # of a clean "bundle missing after upload" message.
                # The probe also records size+mtime in the target log
                # on success, so every future apply failure has
                # forensic upload state alongside the error.
                probe = _probe_remote_bundle(
                    host=host,
                    bundle_path=remote_bundle,
                    ssh_options=ssh_options,
                )
                try:
                    with log_file.open("a", encoding="utf-8") as lf:
                        lf.write(f"bundle post-upload probe: {probe.detail}\n")
                except OSError:
                    pass
                if not probe.exists:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file),
                        f"Bundle upload completed but remote file is "
                        f"missing: {probe.detail}. This is the failure "
                        f"mode from #247 (scp closed cleanly but the "
                        f"file isn't on the remote). Re-run should "
                        f"trigger a fresh upload.",
                    )
                if probe.size == 0:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file),
                        f"Bundle upload completed but remote file is "
                        f"0 bytes: {probe.detail}. This is a silent "
                        f"truncation (#247). Re-run should trigger a "
                        f"fresh upload.",
                    )

                # Apply bundle via PowerShell on the remote. Pass
                # the per-target log_file through so the raw stderr
                # (including any CLIXML envelope) gets persisted
                # before decode, even if bundle apply fails before
                # the streaming/validation layer would normally
                # start writing the log. #200: without this, a
                # bundle-apply-time CLIXML leak leaves zero
                # diagnostic artifact on disk.
                apply_result = _apply_bundle_windows(
                    host=host,
                    bundle_path=remote_bundle,
                    repo_path=remote_repo,
                    ssh_options=ssh_options,
                    timeout=int(target_config.get("bundle_apply_timeout_secs", 1800)),
                    log_file=log_file,
                )
                if not apply_result.success:
                    return _error_result(
                        target_name, platform, started_at, start_time,
                        str(log_file),
                        f"Bundle apply failed: {apply_result.message}",
                    )

        # Step 2: Resolve the Visual Studio toolchain (once per host,
        # cached) so stages can reference $env:SHIPYARD_CMAKE_PLATFORM
        # and $env:SHIPYARD_CMAKE_GENERATOR_INSTANCE. This matches
        # Pulp's local_ci.py behavior for hosts with multiple VS
        # installations or ARM64 Windows. Detection is best-effort:
        # a None result just means CMake falls back to its defaults.
        toolchain = self._get_vs_toolchain(host, ssh_options, target_config)

        # Step 2.5: Resolve resume_from. If the caller asked to skip
        # earlier stages, probe the remote for a marker file written
        # by the previous successful run on this SHA.
        effective_resume = _resolve_resume_from_windows(
            host=host,
            remote_repo=remote_repo,
            sha=sha,
            ssh_options=ssh_options,
            requested=resume_from,
            validation_config=validation_config,
            log_file=log_file,
        )

        # Step 3: Build the remote PowerShell command: env exports +
        # checkout + validate. If the target opts into a host mutex,
        # wrap the whole thing in a Mutex block so concurrent runs
        # against the same Windows host queue up instead of racing.
        command = _build_remote_command(
            sha, remote_repo, validation_config,
            toolchain=toolchain,
            resume_from=effective_resume,
        )
        if not command:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), "No validation command configured",
            )

        if _host_mutex_enabled(target_config):
            command = wrap_powershell_with_host_mutex(
                command,
                mutex_name=target_config.get(
                    "windows_host_mutex_name", DEFAULT_MUTEX_NAME,
                ),
            )

        ssh_cmd = (
            ["ssh"]
            + list(ssh_options)
            + _powershell_encoded_argv(host, command)
        )

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

            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=status,
                backend="ssh-windows",
                duration_secs=result.duration_secs,
                started_at=started_at,
                completed_at=result.completed_at,
                log_path=str(log_file),
                phase=result.phase,
                last_output_at=result.last_output_at,
                error_message=error_message,
                contract_markers_seen=evaluation.seen,
                contract_markers_missing=evaluation.missing,
                contract_violation=evaluation.message,
            )

        except subprocess.TimeoutExpired:
            elapsed = time.monotonic() - start_time
            return TargetResult(
                target_name=target_name,
                platform=platform,
                status=TargetStatus.ERROR,
                backend="ssh-windows",
                duration_secs=elapsed,
                started_at=started_at,
                completed_at=datetime.now(timezone.utc),
                log_path=str(log_file),
                error_message="Validation timed out",
            )

        except OSError as exc:
            return _error_result(
                target_name, platform, started_at, start_time,
                str(log_file), str(exc),
            )

    def _get_vs_toolchain(
        self,
        host: str,
        ssh_options: list[str],
        target_config: dict[str, Any],
    ) -> VsToolchain | None:
        """Return the cached or freshly-detected VS toolchain for a host.

        Respects `windows_vs_detect = false` in the target config to
        opt out entirely. Detection failures are cached as None so a
        missing vswhere doesn't re-probe on every run.

        The cache key is `(host, tuple(ssh_options))` so two targets
        that share a hostname but connect with different SSH options
        (different ports, users, bastions) get independent cache
        entries. Using only the hostname would let the first probe
        pollute subsequent runs against a completely different box.
        """
        if target_config.get("windows_vs_detect", True) is False:
            return None
        cache_key = (host, tuple(ssh_options))
        if cache_key in self._vs_toolchain_cache:
            return self._vs_toolchain_cache[cache_key]
        toolchain = detect_vs_toolchain(host, ssh_options)
        self._vs_toolchain_cache[cache_key] = toolchain
        return toolchain

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Check SSH reachability using the shared probe machinery.

        The remote command is a bare `echo ok` — valid in cmd.exe
        (the default OpenSSH Windows shell), PowerShell, and bash.
        This avoids the pre-#119 failure mode where the probe invoked
        `powershell -Command ...` without `BatchMode=yes`, hung on
        slow Windows handshakes, and lost the error classification.
        """
        from shipyard.executor.ssh import run_probe
        diag = run_probe(target_config, remote_cmd=["echo", "ok"])
        return bool(diag["reachable"])

    def diagnose(self, target_config: dict[str, Any]) -> dict[str, Any]:
        """Rich reachability diagnosis mirroring SSHExecutor.diagnose.

        Categories align with SSHExecutor so agents can branch on the
        same stable set (auth / host_key / network / timeout /
        configuration / unknown) regardless of the Windows-vs-POSIX
        target split.
        """
        from shipyard.executor.ssh import _format_ssh_diagnosis, run_probe
        diag = run_probe(target_config, remote_cmd=["echo", "ok"])
        return {
            "reachable": diag["reachable"],
            "message": _format_ssh_diagnosis(target_config, diag),
            "category": diag["category"],
        }


class _ApplyResult:
    """Minimal result type for the internal apply operation."""

    def __init__(self, success: bool, message: str) -> None:
        self.success = success
        self.message = message


def _is_windows_absolute_path(path: str) -> bool:
    """True if `path` starts with a drive letter or a leading separator.

    Treats `C:\\...`, `C:/...`, `\\\\server\\share\\...`, and `\\foo`
    as absolute; bare names like `shipyard.bundle` or relative
    paths like `Temp\\foo.bundle` as not absolute.
    """
    if not path:
        return False
    if path.startswith("\\\\"):  # UNC
        return True
    if path.startswith("\\") or path.startswith("/"):
        return True
    return len(path) >= 2 and path[1] == ":" and path[0].isalpha()


def _ps_single_quote(value: str) -> str:
    """Escape a string for safe use inside a PowerShell single-quoted literal.

    PowerShell doubles an embedded single quote: `'it''s'`. This
    keeps path-like target config values from breaking the
    `$Bundle = '...'` / `cd '...'` assignments below.
    """
    return value.replace("'", "''")


def _encode_powershell_command(script: str) -> str:
    """Encode a PowerShell script for `powershell -EncodedCommand`.

    PowerShell expects a base64-encoded UTF-16LE byte sequence. This
    is the standard, well-documented way to send a multi-line script
    through any transport (cmd.exe, ssh, anything else) that might
    interpret newlines as command separators.
    """
    return base64.b64encode(script.encode("utf-16-le")).decode("ascii")


def _decode_powershell_command(encoded: str) -> str:
    """Inverse of `_encode_powershell_command`.

    Used by tests so they can assert against the original PS source
    instead of opaque base64. Not used at runtime.
    """
    return base64.b64decode(encoded).decode("utf-16-le")


def decode_encoded_ssh_argv(ssh_argv: list[str]) -> str | None:
    """Public helper: decode the `-EncodedCommand` payload from an ssh argv.

    Returns the original PowerShell script if `ssh_argv` contains
    `-EncodedCommand <payload>`, else None. Tests can use this to
    assert against the original PS source after the executor packs
    it through `_powershell_encoded_argv`.
    """
    try:
        idx = ssh_argv.index("-EncodedCommand")
    except ValueError:
        return None
    if idx + 1 >= len(ssh_argv):
        return None
    return _decode_powershell_command(ssh_argv[idx + 1])


def _powershell_encoded_argv(host: str, script: str) -> list[str]:
    """Build the `[host, powershell, ...]` tail of an ssh argv.

    Always uses `-NoProfile` (skip user profile, faster startup) and
    `-EncodedCommand` (avoids the multi-line drop bug below). Both
    `validate()` and `_apply_bundle_windows()` route through this so
    every PowerShell-over-SSH call goes through the same hardened
    surface.

    History: an earlier version sent the script via
    `powershell -Command <raw>`. That worked for single-line
    scripts but silently dropped every line after the first when
    the script reached PowerShell through Windows OpenSSH's
    cmd.exe shell, because cmd.exe interprets newlines as command
    separators. The 60+-line mutex wrapper would run only its
    first line (a silent `$ErrorActionPreference = 'Stop'`
    assignment), exit 0, and produce zero stdout — which Shipyard
    reported as `pass`. False-greens are the worst class of CI
    bug; `-EncodedCommand` makes the bug structurally impossible.
    """
    return [
        host,
        "powershell",
        "-NoProfile",
        "-NonInteractive",
        "-OutputFormat",
        "Text",
        "-EncodedCommand",
        _encode_powershell_command(script),
    ]


def _remote_head_sha_windows(
    host: str,
    repo_path: str,
    sha: str,
    ssh_options: list[str],
    *,
    timeout: int = 15,
) -> str | None:
    """Return the HEAD SHA from the remote Windows repo, or None on error.

    Used to create incremental bundles: if the remote is at commit X
    and we need to deliver commit Y, the bundle only needs objects
    reachable from Y but not from X. For a typical 1-2 commit delta
    this reduces a 443 MB bundle to a few KB.

    Returns None on any error so the caller falls through to a full
    bundle as a safe default.
    """
    safe_repo = _ps_single_quote(repo_path)
    script = f"cd '{safe_repo}'; git rev-parse HEAD"
    cmd = ["ssh"] + list(ssh_options) + _powershell_encoded_argv(host, script)
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
        if result.returncode == 0:
            head = result.stdout.strip()
            if head and len(head) >= 7 and all(c in "0123456789abcdef" for c in head):
                return head
        return None
    except (subprocess.SubprocessError, OSError):
        return None


def _remote_has_sha_windows(
    host: str,
    repo_path: str,
    sha: str,
    ssh_options: list[str],
    *,
    timeout: int = 15,
) -> bool:
    """Check whether the remote Windows repo already contains the SHA.

    Uses `git cat-file -e` via the EncodedCommand transport (same as
    all other Windows SSH calls). If the object exists, the caller
    skips the bundle create → scp upload → apply pipeline — saving
    10–30 minutes on large repos where scp + SFTP close hangs are
    the dominant cost.

    Returns False on any error so the caller falls through to the
    bundle path as a safe default.
    """
    safe_repo = _ps_single_quote(repo_path)
    safe_sha = _ps_single_quote(sha)
    script = f"cd '{safe_repo}'; git cat-file -e '{safe_sha}'; exit $LASTEXITCODE"
    cmd = ["ssh"] + list(ssh_options) + _powershell_encoded_argv(host, script)
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
        return result.returncode == 0
    except (subprocess.SubprocessError, OSError):
        return False


class _BundleProbe:
    """Result of a post-upload bundle-on-remote check (#247).

    ``exists`` reflects the result of ``Test-Path -LiteralPath``.
    ``size`` is the file's length in bytes (0 when the file is
    missing or we couldn't parse the Get-Item output).
    ``detail`` is a short human-readable summary suitable for logs
    and error messages.
    """

    def __init__(self, exists: bool, size: int, detail: str) -> None:
        self.exists = exists
        self.size = size
        self.detail = detail


def _probe_remote_bundle(
    host: str,
    bundle_path: str,
    ssh_options: list[str],
    *,
    timeout: int = 30,
) -> _BundleProbe:
    """Check that the uploaded bundle actually landed on the remote
    host, and capture its size + mtime as a forensic artifact (#247).

    ``upload_bundle`` returns success when scp/ssh closes cleanly —
    but a cleanly-closed channel can still end up with zero bytes on
    disk (session dropout, remote-side write failure the SFTP server
    didn't surface). Without this probe the next diagnostic signal
    is git's ``error: could not open '...'``, which gets wrapped by
    PowerShell's CLIXML envelope and interleaved with progress
    records — so the user sees the Spectr / pulp#728 failure shape
    instead of a clean "bundle missing after upload" message.

    Runs a single SSH command (Test-Path + Get-Item if present) and
    returns an opaque probe object the caller decides what to do
    with. Any SSH/PowerShell failure returns ``exists=False`` with a
    diagnostic ``detail`` string — the caller treats that as "upload
    verification failed, bail" rather than guessing.
    """
    safe_bundle = _ps_single_quote(bundle_path)
    resolved = (
        f"'{safe_bundle}'"
        if _is_windows_absolute_path(bundle_path)
        else f"(Join-Path $HOME '{safe_bundle}')"
    )
    script = (
        f"{_WINDOWS_UTF8_PRELUDE}"
        f"$Bundle = {resolved}; "
        f"if (Test-Path -LiteralPath $Bundle) {{ "
        f"$i = Get-Item -LiteralPath $Bundle; "
        # One-line, whitespace-separated; easy to parse and easy to
        # eyeball in the log. mtime in ISO-8601 UTC.
        f"Write-Output (\"OK size=\" + $i.Length + "
        f"\" mtime=\" + $i.LastWriteTimeUtc.ToString('o') + "
        f"\" path=\" + $Bundle) "
        f"}} else {{ "
        f"Write-Output (\"MISSING path=\" + $Bundle) "
        f"}}"
    )
    cmd = ["ssh"] + list(ssh_options) + _powershell_encoded_argv(host, script)
    try:
        result = subprocess.run(
            cmd, capture_output=True, text=True, timeout=timeout,
        )
    except subprocess.TimeoutExpired:
        return _BundleProbe(
            exists=False, size=0,
            detail=f"probe timed out after {timeout}s",
        )
    except (subprocess.SubprocessError, OSError) as exc:
        return _BundleProbe(
            exists=False, size=0, detail=f"probe error: {exc}",
        )
    if result.returncode != 0:
        stderr_snip = (result.stderr or "").strip()[:200]
        return _BundleProbe(
            exists=False, size=0,
            detail=(
                f"probe exited {result.returncode}"
                + (f": {stderr_snip}" if stderr_snip else "")
            ),
        )
    stdout = (result.stdout or "").strip()
    # Scan for our sentinel lines anywhere in stdout rather than
    # requiring them at the start. Some users configure a PowerShell
    # profile that prints a banner on session open; requiring our
    # line to be position-0 would false-positive fail on those hosts
    # and regress them relative to pre-#247 behavior.
    import re
    ok_match = re.search(
        r"OK\s+size=(\d+)\s+mtime=(\S+)\s+path=(.+)", stdout,
    )
    if ok_match:
        size = int(ok_match.group(1))
        # Keep `detail` as the matched line only, not the whole
        # stdout — makes the log readable even when a banner runs
        # long.
        detail = (
            f"OK size={size} mtime={ok_match.group(2)} "
            f"path={ok_match.group(3).strip()}"
        )
        return _BundleProbe(exists=True, size=size, detail=detail)
    missing_match = re.search(r"MISSING\s+path=(.+)", stdout)
    if missing_match:
        detail = f"MISSING path={missing_match.group(1).strip()}"
        return _BundleProbe(exists=False, size=0, detail=detail)
    # Neither sentinel found — treat as "can't verify" and surface
    # the snippet so the operator can diagnose.
    return _BundleProbe(
        exists=False, size=0,
        detail=f"probe unexpected output: {stdout[:200]!r}",
    )


def _apply_bundle_windows(
    host: str,
    bundle_path: str,
    repo_path: str,
    ssh_options: list[str],
    timeout: int = 1800,
    log_file: Path | None = None,
) -> _ApplyResult:
    """Apply a git bundle on a remote Windows host via PowerShell.

    Fetches bundle refs into a Shipyard-owned namespace rather
    than `refs/*`. The naive `+refs/*:refs/*` mapping fails with
    "refusing to fetch into branch <name> checked out at <path>"
    whenever the remote worktree happens to have the bundled
    branch checked out. The namespaced destination is never a
    checked-out ref, so git accepts the fetch unconditionally.

    The bundle path is resolved via PowerShell's Join-Path with
    $HOME when it's a relative path, so the default
    `shipyard.bundle` lands in the SSH user's home directory (where
    scp wrote it) regardless of the working directory PowerShell
    uses after the `cd` below.

    The apply timeout defaults to 30 minutes (matching
    upload_bundle) so `git bundle verify` + `git fetch` on a large
    repo doesn't get killed on slow Windows disks. The previous
    120s was too tight for anything with real history.

    On failure, the raw stderr (CLIXML envelope + exit code) is
    written to a sibling log file — ``<log_file>.bundle-apply-stderr``
    — before attempting decode. #200: the CLIXML decoder was
    already wired in (#189) but real bundle-apply failures on pulp
    surfaced only the sentinel ``#< CLIXML`` with no body or log
    artifact to analyze. Persisting raw stderr gives every
    future failure a self-describing forensic record regardless of
    whether the decoder hits a complete envelope, a truncated one,
    or something that isn't CLIXML at all.
    """
    # Expand a relative bundle path against $HOME inside PowerShell.
    # Detecting "relative" on the Windows side is simpler than on
    # the Python side because we can just check for a drive letter
    # or backslash prefix.
    safe_bundle = _ps_single_quote(bundle_path)
    safe_repo = _ps_single_quote(repo_path)
    resolved = (
        f"'{safe_bundle}'"
        if _is_windows_absolute_path(bundle_path)
        else f"(Join-Path $HOME '{safe_bundle}')"
    )

    ps_cmd = (
        # UTF-8 prelude (#208): also affects git bundle verify /
        # git fetch output and any non-ASCII bundle path.
        f"{_WINDOWS_UTF8_PRELUDE}"
        f"$Bundle = {resolved}; "
        # #210: pre-verify the bundle exists at the resolved path
        # before handing it to git. If the upload landed the file
        # somewhere else (SSH session working-dir drift, relative-
        # path mismatch, etc.), `git bundle verify` produces
        # "error: could not open '...'" which arrives on stderr
        # BEFORE PowerShell's CLIXML envelope starts, so the decoder
        # has nothing to extract and the user saw only `#< CLIXML`.
        # Pre-check surfaces a clean diagnostic naming the exact
        # expected path.
        f"if (-not (Test-Path -LiteralPath $Bundle)) {{ "
        f"Write-Error \"shipyard: bundle file not found at $Bundle \"\""
        f"(expected after scp/ssh upload; check upload step logs)\"; "
        f"exit 1 }}; "
        f"cd '{safe_repo}'; "
        f"git bundle verify $Bundle; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}; "
        f"git fetch $Bundle "
        f"'+refs/heads/*:refs/shipyard-bundles/heads/*' "
        f"'+refs/tags/*:refs/shipyard-bundles/tags/*'; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}"
    )

    cmd = ["ssh"] + list(ssh_options) + _powershell_encoded_argv(host, ps_cmd)

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if result.returncode != 0:
            raw_stderr = result.stderr or ""
            # Persist raw stderr BEFORE decode (#200). The previous
            # comment here claimed the envelope was saved by the
            # streaming layer, but bundle-apply failures happen
            # before streaming/validation starts — no streaming
            # capture ever runs, the envelope was lost. Now we
            # write it next to where the target log would be and
            # reference it in the error message so the user has a
            # concrete artifact to inspect.
            stderr_log_path: Path | None = None
            if log_file is not None:
                try:
                    stderr_log_path = Path(str(log_file) + ".bundle-apply-stderr")
                    stderr_log_path.parent.mkdir(parents=True, exist_ok=True)
                    stderr_log_path.write_text(
                        f"=== exit_code={result.returncode} ===\n"
                        f"=== stderr (bytes={len(raw_stderr)}) ===\n"
                        f"{raw_stderr}\n"
                        f"=== stdout (bytes={len(result.stdout or '')}) ===\n"
                        f"{result.stdout or ''}\n",
                    )
                except OSError:
                    # Persisting the log is best-effort — the primary
                    # failure path must not be hidden by a log-write
                    # error. Fall through to the original error
                    # message without the log reference.
                    stderr_log_path = None

            # PowerShell relays stderr as a CLIXML envelope (#188).
            # Decoded text names the actual cause when the envelope
            # is complete; falls back to raw when it's truncated or
            # malformed (see #200 for the real-world truncation case).
            detail = maybe_decode_clixml(raw_stderr.strip())
            message = f"Remote bundle apply failed: {detail}"
            if stderr_log_path is not None:
                message += f" (raw stderr: {stderr_log_path})"
            return _ApplyResult(success=False, message=message)
        return _ApplyResult(success=True, message="Bundle applied")

    except subprocess.TimeoutExpired:
        return _ApplyResult(success=False, message="Remote bundle apply timed out")
    except OSError as exc:
        return _ApplyResult(success=False, message=f"OS error: {exc}")


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
    """Marker file written to the remote repo after a stage succeeds."""
    return f".shipyard-stage-{stage}-{sha[:12]}"


def _resolve_resume_from_windows(
    *,
    host: str,
    remote_repo: str,
    sha: str,
    ssh_options: list[str],
    requested: str | None,
    validation_config: dict[str, Any],
    log_file: Path,
) -> str | None:
    """Decide whether to honor a `--resume-from` request on Windows.

    Mirrors the POSIX behavior: only honor when the previous stage's
    marker file exists on the remote for this exact SHA.
    """
    if requested is None:
        return None
    if "command" in validation_config:
        _append_log(
            log_file,
            f"=== resume-from: ignored ({requested!r}) — "
            f"validation_config uses single command ===\n",
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
        return requested
    prev_stage = None
    for candidate in STAGE_ORDER[:idx][::-1]:
        if validation_config.get(candidate):
            prev_stage = candidate
            break
    if prev_stage is None:
        return requested

    marker = _stage_marker_name(prev_stage, sha)
    if _remote_marker_exists_windows(
        host, remote_repo, marker, ssh_options,
    ):
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


def _remote_marker_exists_windows(
    host: str,
    repo_path: str,
    marker: str,
    ssh_options: list[str],
    *,
    timeout: int = 15,
) -> bool:
    """Check whether `<repo>\\<marker>` exists on the remote Windows host."""
    safe_repo = _ps_single_quote(repo_path)
    safe_marker = _ps_single_quote(marker)
    script = (
        f"if (Test-Path (Join-Path '{safe_repo}' '{safe_marker}')) "
        f"{{ exit 0 }} else {{ exit 1 }}"
    )
    cmd = ["ssh"] + list(ssh_options) + _powershell_encoded_argv(host, script)
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


# Prelude prepended to every PowerShell command shipyard dispatches to
# a Windows host. Forces UTF-8 across the Win32 console round-trip so
# non-ASCII argv (em-dashes, emoji, accented test names) survive the
# trip through any child process.
#
# Three settings cover three different encoding paths:
#
#   `chcp.com 65001`          → sets the Win32 console code page.
#                                Child processes' CRT and Win32 APIs
#                                (GetConsoleCP, GetConsoleOutputCP)
#                                pick this up. Without it, Namespace's
#                                Windows image defaults to CP-1252 and
#                                UTF-8 argv → CP-1252 → mojibake.
#   `[Console]::OutputEncoding`→ how PowerShell decodes child-process
#                                stdout back into strings.
#   `$OutputEncoding`          → how PowerShell sends pipeline data
#                                TO a child process's stdin.
#
# All three are session-scoped — set once at the top of the PS
# script, they apply to everything that session spawns, and die when
# the session does. No leak, no global mutation.
#
# Zero-cost on hosts already at 65001 (GitHub-hosted Windows runners
# default to it); the setters are idempotent. See #208 for the
# concrete incident: pulp's ctest hit 46 spurious failures on
# Namespace Windows because em-dash test names got mangled.
#
# ``| Out-Null`` suppresses `chcp`'s "Active code page: 65001"
# confirmation line so it doesn't pollute the validation log.
_WINDOWS_UTF8_PRELUDE = (
    "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; "
    "$OutputEncoding = [System.Text.Encoding]::UTF8; "
    "chcp.com 65001 | Out-Null; "
)


def _build_remote_command(
    sha: str,
    remote_repo: str,
    validation_config: dict[str, Any],
    *,
    toolchain: VsToolchain | None = None,
    resume_from: str | None = None,
) -> str | None:
    """Build the remote PowerShell command: cd + checkout + validate.

    Uses semicolons for PowerShell command chaining with $LASTEXITCODE checks.
    When `toolchain` is provided, the resolved CMake platform and generator
    instance are exported as env vars so stages can reference them.

    When ``resume_from`` is set, stages before the resume point are
    skipped. After each stage succeeds, a marker file is written so
    a subsequent resume run can detect the earlier stage's success.
    """
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
            safe_marker = _ps_single_quote(marker)
            parts.append(
                f"Write-Output '__SHIPYARD_PHASE__:{step}'; {cmd}; "
                f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}; "
                f"New-Item -ItemType File -Force -Path '{safe_marker}' | Out-Null"
            )
        if not parts:
            return None
        # Chain with PowerShell error checking
        validate_cmd = "; if ($LASTEXITCODE -ne 0) { exit 1 }; ".join(parts)

    # Escape single quotes in the repo path so a pathological
    # target_config value can't break out of the PS literal.
    safe_repo = _ps_single_quote(remote_repo)
    safe_sha = _ps_single_quote(sha)
    return (
        f"{_WINDOWS_UTF8_PRELUDE}"
        f"{toolchain_env_exports(toolchain)}; "
        f"cd '{safe_repo}'; "
        f"git checkout --force '{safe_sha}'; "
        f"if ($LASTEXITCODE -ne 0) {{ exit 1 }}; "
        f"{validate_cmd}"
    )


def _host_mutex_enabled(target_config: dict[str, Any]) -> bool:
    """Read the host-mutex opt-in from the target config.

    Default: True. Concurrent validation runs on a Windows host share
    a checked-out repo and a locked VS install, so serializing by
    default prevents hard-to-diagnose flaky failures. Projects that
    use per-job worktrees can disable it with
    `windows_host_mutex = false` in the target config.
    """
    return target_config.get("windows_host_mutex", True) is not False


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
        backend="ssh-windows",
        duration_secs=time.monotonic() - start_time,
        started_at=started_at,
        completed_at=datetime.now(timezone.utc),
        log_path=log_path,
        error_message=message,
    )


def _extract_ssh_error(output: str) -> str | None:
    # PowerShell wraps any stderr stream in a CLIXML envelope; the
    # actual message lives inside. Decode before falling through to
    # the bare-last-line heuristic so ssh_windows errors read the
    # same as ssh ones under the summary table. See #188.
    decoded = maybe_decode_clixml(output)
    for line in reversed(decoded.splitlines()):
        if line.strip():
            return line.strip()
    return None
