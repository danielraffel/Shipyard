"""Git bundle operations for delivering code to remote hosts.

Creates a git bundle from the local repo, uploads it via SCP, and applies
it on the remote side. This avoids needing the remote to have git credentials
or access to the upstream repository.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Sequence


@dataclass(frozen=True)
class BundleResult:
    """Outcome of a bundle operation.

    `failure_class` (#239 Phase A): on upload failures, categorizes
    the root cause so callers can surface it and retry-deciders can
    route:
      - ``ssh_unreachable``: raw SSH connect failed (port 22 timeout,
        connection refused). Retrying the upload won't help on its
        own — the network / host is down.
      - ``upload_failed``: SSH reached the host but the upload itself
        exited non-zero / timed out / broke mid-stream. Could be
        transient slow-runner behavior; retry-with-backoff helps.
      - ``other``: bundle creation failure, caller misconfig, etc.
        Preserved for backward compat.
    `attempts` carries per-attempt stderr + wall time on upload
    paths — without this, the user sees one Upload-failed message
    with no sense of whether attempt 1 failed fast and attempt 2
    timed out or vice versa.
    """

    success: bool
    message: str
    path: str | None = None
    failure_class: str = "other"
    attempts: tuple[str, ...] = ()


def create_bundle(
    sha: str,
    output_path: str | Path,
    repo_dir: str | Path | None = None,
    basis_shas: Sequence[str] = (),
) -> BundleResult:
    """Create a git bundle containing the given SHA and its ancestors.

    Args:
        sha: The commit SHA to include (up to and including this commit).
        output_path: Local filesystem path for the generated .bundle file.
        repo_dir: Working directory for git commands. Defaults to cwd.
        basis_shas: SHAs the remote already has. Each is passed as
            ``^<sha>`` to ``git bundle create``, producing an incremental
            bundle that excludes objects reachable from those commits.
            When empty, ``--all`` is used to create a full bundle.

    Returns:
        BundleResult indicating success or failure.
    """
    output_path = Path(output_path)
    output_path.parent.mkdir(parents=True, exist_ok=True)

    cwd = str(repo_dir) if repo_dir else None

    cmd = ["git", "bundle", "create", str(output_path), sha]
    if basis_shas:
        for basis in basis_shas:
            cmd.append(f"^{basis}")
    else:
        cmd.append("--all")

    try:
        result = subprocess.run(
            cmd,
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=120,
        )
        if result.returncode != 0:
            return BundleResult(
                success=False,
                message=f"git bundle create failed: {result.stderr.strip()}",
            )
        return BundleResult(
            success=True,
            message="Bundle created",
            path=str(output_path),
        )

    except subprocess.TimeoutExpired:
        return BundleResult(success=False, message="git bundle create timed out")
    except OSError as exc:
        return BundleResult(success=False, message=f"OS error: {exc}")


_SSH_UNREACHABLE_FINGERPRINTS = (
    # Raw ssh/connect failures — host is down, DNS failed, port
    # filtered, or TCP handshake timed out. Retrying the upload
    # alone won't help on its own; the caller may want to probe
    # connectivity separately before retrying.
    "connect to host",           # "ssh: connect to host X port 22: …"
    "connection refused",        # daemon down / wrong port
    "no route to host",          # network partition
    "name or service not known",  # DNS
    "operation timed out",       # TCP handshake timeout
    "network is unreachable",
)


def _classify_upload_failure(stderr: str) -> str:
    """Return 'ssh_unreachable' / 'upload_failed' based on stderr
    fingerprints. See BundleResult.failure_class docstring.
    """
    low = stderr.lower()
    for fp in _SSH_UNREACHABLE_FINGERPRINTS:
        if fp in low:
            return "ssh_unreachable"
    return "upload_failed"


def upload_bundle(
    bundle_path: str | Path,
    host: str,
    remote_path: str,
    ssh_options: Sequence[str] = (),
    timeout: int = 1800,
    *,
    is_windows: bool = False,
    max_attempts: int = 3,
) -> BundleResult:
    """Upload a bundle file to a remote host via SCP.

    Args:
        bundle_path: Local path to the .bundle file.
        host: SSH host (user@host or alias).
        remote_path: Destination path on the remote host.
        ssh_options: Additional SSH/SCP options (e.g. ["-o", "StrictHostKeyChecking=no"]).
        timeout: scp timeout in seconds. Defaults to 30 minutes to
            accommodate large repos over slow links (e.g. Pulp's
            ~100MB bundle to a Windows VM). Callers with known
            small bundles can pass a shorter timeout; 5 minutes was
            the previous default and turned out to be too
            aggressive for real workloads.
        max_attempts: Retry budget per #239 Phase A. Defaults to 3
            so a slow-runner hiccup (Pulp repro: Windows runner
            under AV-scan pressure) doesn't fail the lane on the
            first transient. When the first attempt fingerprints
            as ``ssh_unreachable`` we skip further retries —
            retrying the same unreachable host won't help and we
            want to fail fast.

    Returns:
        BundleResult indicating success or failure. On failure,
        ``failure_class`` carries the classification and
        ``attempts`` carries per-attempt stderr.
    """
    bundle_path = Path(bundle_path)
    if not bundle_path.exists():
        return BundleResult(
            success=False,
            message=f"Bundle file not found: {bundle_path}",
        )

    # Use `ssh cat > file` instead of scp. Windows OpenSSH's SFTP
    # subsystem hangs during session close after large transfers
    # (443 MB+), causing scp to stall indefinitely even though the
    # file has fully arrived. Piping through `ssh cat >` bypasses the
    # SFTP subsystem entirely — the data goes through the SSH channel's
    # stdin/stdout, which closes cleanly. This is also faster for
    # large files because there's no SFTP protocol overhead.
    #
    # The remote command uses `cat > path` on POSIX hosts and
    # `powershell -Command [IO.File]::WriteAllBytes(...)` is NOT
    # needed — Windows cmd.exe's `type con > file` doesn't work, but
    # `ssh host "cat > file"` works because OpenSSH pipes stdin to
    # the remote shell's stdin, and both cmd.exe and PowerShell can
    # redirect stdin to a file via shell builtins. We use the simplest
    # form that works on both: the remote sees binary stdin and `cat`
    # writes it. On Windows, `cmd /c "more > file"` or similar won't
    # work for binary, so we explicitly invoke PowerShell for binary-
    # safe stdin capture.
    #
    # Detection: if remote_path contains a backslash or a drive letter,
    # assume Windows and use PowerShell; otherwise use cat.
    is_windows = is_windows or "\\" in remote_path or (
        len(remote_path) >= 2 and remote_path[1] == ":"
    )

    if is_windows:
        # PowerShell binary-safe stdin → file.
        #
        # #210: resolve relative `remote_path` against `$HOME` inside
        # the PS script before creating the file. Otherwise the file
        # lands in whatever directory OpenSSH's server spawned the
        # shell in (SSHD default = $HOME, but various configs
        # deviate) and the apply step — which Join-Paths against
        # $HOME explicitly — then fails with "could not open" because
        # the two sides disagree on what the "relative" base is.
        # Anchoring at $HOME on both sides makes the bundle location
        # deterministic regardless of SSHD config.
        #
        # Contract must match `_is_windows_absolute_path` in
        # `executor/ssh_windows.py` — apply-side uses that predicate
        # to decide whether to quote the bundle path as-is or join
        # it to $HOME. If upload and apply disagree on what counts as
        # "absolute," slash-prefixed paths like `/tmp/x.bundle` get
        # written to `$HOME/tmp/x.bundle` on upload but read from
        # `/tmp/x.bundle` on apply, reproducing the exact #210 bug
        # for a different path shape (Codex P1 on #211).
        # The PS single-quote literal we build below doesn't escape
        # embedded `'`. Reject paths that contain one — that's
        # script-injection surface on the rooted branch and a silent
        # syntax break on both. Codex P2 on #213 caught that my #211
        # fix only checked the relative branch; before then, rooted
        # paths couldn't reach this code at all, so my change made it
        # a regression. Apply the guard uniformly by hoisting it.
        if "'" in remote_path:
            return BundleResult(
                success=False,
                message=f"Refusing single-quoted remote_path: {remote_path!r}",
            )
        is_rooted = (
            remote_path.startswith("\\\\")
            or remote_path.startswith("\\")
            or remote_path.startswith("/")
            or (
                len(remote_path) >= 2
                and remote_path[1] == ":"
                and remote_path[0].isalpha()
            )
        )
        resolved_dest = (
            f"'{remote_path}'" if is_rooted
            else f"(Join-Path $HOME '{remote_path}')"
        )
        ps_script = (
            f"$Dest = {resolved_dest};"
            f"$stdin = [Console]::OpenStandardInput();"
            f"$fs = [System.IO.File]::Create($Dest);"
            f"$stdin.CopyTo($fs);"
            f"$fs.Close();"
            f"$stdin.Close()"
        )
        import base64
        encoded = base64.b64encode(ps_script.encode("utf-16-le")).decode("ascii")
        cmd: list[str] = ["ssh"]
        for opt in ssh_options:
            cmd.append(opt)
        cmd.extend([host, "powershell", "-NoProfile", "-EncodedCommand", encoded])
    else:
        cmd = ["ssh"]
        for opt in ssh_options:
            cmd.append(opt)
        cmd.extend([host, f"cat > {remote_path}"])

    # Retry with backoff + jitter on transient upload failures
    # (#239 Phase A). Slow Windows runners under AV/indexing
    # pressure hit ~60s PowerShell startup latency, which can blow
    # the upload's connect phase even though the host is otherwise
    # reachable. Three attempts with 2s / 5s backoff gives us a
    # real shot at completing without retry budget becoming
    # wall-clock-significant on healthy runs.
    import random as _random
    import time as _time

    attempts_log: list[str] = []
    last_stderr = ""
    last_exception_msg = ""
    final_class = "upload_failed"

    for attempt in range(1, max_attempts + 1):
        attempt_start = _time.monotonic()
        try:
            with open(bundle_path, "rb") as f:
                result = subprocess.run(
                    cmd,
                    stdin=f,
                    capture_output=True,
                    text=True,
                    timeout=timeout,
                )
            elapsed = _time.monotonic() - attempt_start
            if result.returncode == 0:
                if attempt > 1:
                    attempts_log.append(
                        f"attempt {attempt}/{max_attempts}: "
                        f"success after {elapsed:.1f}s"
                    )
                return BundleResult(
                    success=True,
                    message="Bundle uploaded",
                    path=remote_path,
                    attempts=tuple(attempts_log),
                )
            last_stderr = (result.stderr or "").strip()
            attempts_log.append(
                f"attempt {attempt}/{max_attempts} failed "
                f"after {elapsed:.1f}s: {last_stderr[:200] or '(no stderr)'}"
            )
            final_class = _classify_upload_failure(last_stderr)
            # Fail-fast on unreachable: retrying won't help if the
            # TCP connect itself is broken. Let the caller see the
            # classification quickly so they can surface it without
            # burning the full retry budget on a dead host.
            if final_class == "ssh_unreachable":
                break
        except subprocess.TimeoutExpired:
            elapsed = _time.monotonic() - attempt_start
            # Phrase as "timed out after" (past tense) so callers
            # grepping for the word "timeout" OR "timed out" both
            # match — the existing BundleResult contract tests
            # assert the substring "timed out" so we preserve it
            # verbatim.
            last_stderr = (
                f"timed out after {elapsed:.1f}s (budget {timeout}s)"
            )
            attempts_log.append(
                f"attempt {attempt}/{max_attempts}: {last_stderr}"
            )
            final_class = "upload_failed"
        except OSError as exc:
            last_exception_msg = f"OS error: {exc}"
            attempts_log.append(
                f"attempt {attempt}/{max_attempts}: {last_exception_msg}"
            )
            return BundleResult(
                success=False,
                message=last_exception_msg,
                failure_class="other",
                attempts=tuple(attempts_log),
            )

        # Backoff before the next attempt (unless this is the last).
        # 2s base + 0-50% jitter on attempt 1→2, 5s on 2→3.
        if attempt < max_attempts:
            base = 2.0 if attempt == 1 else 5.0
            _time.sleep(_random.uniform(base, base * 1.5))

    # All attempts exhausted.
    summary = last_stderr or last_exception_msg or "no stderr captured"
    return BundleResult(
        success=False,
        message=(
            f"Upload failed after {len(attempts_log)} attempt(s): {summary}"
        ),
        failure_class=final_class,
        attempts=tuple(attempts_log),
    )


def apply_bundle(
    host: str,
    bundle_path: str,
    repo_path: str,
    ssh_options: Sequence[str] = (),
    timeout: int = 1800,
) -> BundleResult:
    """Apply a git bundle on a remote host via SSH.

    Fetches bundle refs into a Shipyard-owned namespace
    (`refs/shipyard-bundles/*`) rather than `refs/*`. The naive
    `+refs/*:refs/*` mapping fails with "refusing to fetch into
    branch <name> checked out at <path>" whenever the remote
    worktree happens to have the bundled branch checked out —
    which is extremely common on a long-lived validation VM. The
    namespaced destination is never a checked-out ref, so git
    accepts the fetch unconditionally.

    The validation layer walks `refs/shipyard-bundles/*` to find
    the exact SHA; the remote checkout will be done separately by
    the executor's per-target logic.

    Args:
        host: SSH host (user@host or alias).
        bundle_path: Path to the .bundle file on the remote host.
        repo_path: Path to the git repo on the remote host.
        ssh_options: Additional SSH options.
        timeout: Apply timeout in seconds. Defaults to 30 minutes
            so `git bundle verify` + `git fetch` of a large repo
            doesn't get killed on slow Windows disks. The previous
            120s default was fine for small repos but too tight
            for anything with real history; raising it matched the
            upload_bundle default.

    Returns:
        BundleResult indicating success or failure.
    """
    # Quote path-like arguments that get interpolated into a shell
    # command. `repo_path` and `bundle_path` can come from target
    # config and may contain spaces, quotes, or shell metacharacters.
    import shlex
    quoted_repo = shlex.quote(repo_path)
    quoted_bundle = shlex.quote(bundle_path)
    remote_cmd = (
        f"cd {quoted_repo} && "
        f"git bundle verify {quoted_bundle} && "
        f"git fetch {quoted_bundle} "
        f"'+refs/heads/*:refs/shipyard-bundles/heads/*' "
        f"'+refs/tags/*:refs/shipyard-bundles/tags/*'"
    )

    cmd: list[str] = ["ssh"]
    for opt in ssh_options:
        cmd.append(opt)
    cmd.extend([host, remote_cmd])

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        if result.returncode != 0:
            return BundleResult(
                success=False,
                message=f"Remote bundle apply failed: {result.stderr.strip()}",
            )
        return BundleResult(
            success=True,
            message="Bundle applied",
            path=bundle_path,
        )

    except subprocess.TimeoutExpired:
        return BundleResult(success=False, message="Remote bundle apply timed out")
    except OSError as exc:
        return BundleResult(success=False, message=f"OS error: {exc}")
