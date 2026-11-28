#!/usr/bin/env python3
"""Run the release cargo build under a hard wall-clock limit.

On 2026-11-28 the v0.464.0 release build sat at 0% CPU with no open sockets
for over an hour on a self-hosted Mac, inside a step whose only bound was the
115-minute step timeout, and nothing recorded why. A day earlier the same build
failed on a crates.io "transfer too slow" error. This module runs the build
with:

- a hard limit (``SHIPYARD_RELEASE_BUILD_TIMEOUT_MINUTES``, default 20);
- cargo network retries and an explicit low-speed window, so a slow crates.io
  transfer is retried rather than fatal;
- stdin from /dev/null and ``GIT_TERMINAL_PROMPT=0``, so no prompt can wait;
- timestamped output with cargo's progress bar off, so the log shows where
  time went.

When the limit is hit it records, before killing anything, the build's own
process tree, its open files and sockets, a ``sample`` of the stuck processes,
the last output lines and the default keychain's state, into the job log and
``SHIPYARD_RELEASE_BUILD_DIAGNOSTICS_DIR``. Only the build's own processes are
listed (never the whole host), and every line is redacted.
"""

from __future__ import annotations

import collections
import contextlib
import os
import pwd
import signal
import subprocess
import sys
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Callable, Iterable

DEFAULT_TIMEOUT_MINUTES = 20.0
TIMEOUT_ENV = "SHIPYARD_RELEASE_BUILD_TIMEOUT_MINUTES"
DIAGNOSTICS_ENV = "SHIPYARD_RELEASE_BUILD_DIAGNOSTICS_DIR"
TAIL_LINES = 200
# Per-command caps for the diagnostics. Their sum, plus the kill grace, is the
# budget the workflow step timeout leaves above the build limit
# (scripts/test_release_workflow.py checks it).
PS_TIMEOUT_SECS = 15
LSOF_TIMEOUT_SECS = 60
SAMPLE_SECONDS = 3
SAMPLE_TIMEOUT_SECS = 30
MAX_SAMPLED = 4
SECURITY_TIMEOUT_SECS = 15
KILL_GRACE_SECS = 5.0
KILL_WAIT_SECS = 30.0
DIAGNOSTICS_BUDGET_SECS = (
    PS_TIMEOUT_SECS
    + LSOF_TIMEOUT_SECS
    + MAX_SAMPLED * SAMPLE_TIMEOUT_SECS
    + 3 * SECURITY_TIMEOUT_SECS
    + KILL_GRACE_SECS
    + KILL_WAIT_SECS
)

# Defaults only: a value the operator set in the environment wins.
# cargo's own defaults are 3 retries and a 30 s window under 10 bytes/s; the
# v0.461.0 failure was that window tripping once on a slow crates.io transfer.
CARGO_NETWORK_ENV = {
    "CARGO_NET_RETRY": "10",
    "CARGO_HTTP_TIMEOUT": "60",
    "CARGO_HTTP_LOW_SPEED_LIMIT": "10",
    "CARGO_TERM_PROGRESS_WHEN": "never",
    "CARGO_TERM_COLOR": "never",
    "GIT_TERMINAL_PROMPT": "0",
}


class BuildTimedOut(SystemExit):
    """The build hit its wall-clock limit; diagnostics were written first."""


class BuildFailed(SystemExit):
    """The build exited non-zero."""


@dataclass(frozen=True)
class ProcessRow:
    pid: int
    ppid: int
    pgid: int
    line: str


def timeout_seconds(environ: dict[str, str] | None = None) -> float:
    environ = os.environ if environ is None else environ
    raw = environ.get(TIMEOUT_ENV, "").strip()
    if not raw:
        return DEFAULT_TIMEOUT_MINUTES * 60
    try:
        minutes = float(raw)
    except ValueError as error:
        raise SystemExit(f"{TIMEOUT_ENV} must be a number of minutes, got {raw!r}") from error
    if minutes <= 0:
        raise SystemExit(f"{TIMEOUT_ENV} must be positive, got {raw!r}")
    return minutes * 60


def build_env(base: dict[str, str] | None = None) -> dict[str, str]:
    env = dict(os.environ if base is None else base)
    for name, value in CARGO_NETWORK_ENV.items():
        env.setdefault(name, value)
    return env


def with_verbose(args: list[str]) -> list[str]:
    """Add cargo's ``-v`` right after the subcommand so each step is named."""
    if len(args) >= 2 and Path(args[0]).name == "cargo" and "-v" not in args:
        return [args[0], args[1], "-v", *args[2:]]
    return list(args)


def parse_ps(output: str) -> list[ProcessRow]:
    rows: list[ProcessRow] = []
    for raw in output.splitlines():
        parts = raw.split(None, 3)
        if len(parts) < 3:
            continue
        try:
            pid, ppid, pgid = (int(p) for p in parts[:3])
        except ValueError:
            continue
        rows.append(ProcessRow(pid, ppid, pgid, raw.rstrip()))
    return rows


def build_tree(rows: Iterable[ProcessRow], root: int) -> list[ProcessRow]:
    """``root`` and every descendant, plus orphans left in its process group.

    Nothing else on the host is returned: a shared runner runs other users'
    jobs, whose command lines may carry credentials.
    """
    rows = list(rows)
    children: dict[int, list[ProcessRow]] = collections.defaultdict(list)
    for row in rows:
        children[row.ppid].append(row)
    keep: dict[int, ProcessRow] = {}
    stack = [row for row in rows if row.pid == root]
    while stack:
        row = stack.pop()
        if row.pid in keep:
            continue
        keep[row.pid] = row
        stack.extend(children.get(row.pid, []))
    for row in rows:
        if row.pgid == root and row.pid not in keep:
            keep[row.pid] = row
    return [keep[pid] for pid in sorted(keep)]


def sample_targets(tree: list[ProcessRow], root: int, limit: int = MAX_SAMPLED) -> list[int]:
    """Leaves first (a build script, cc, ld or xcrun waiting on something), then the root."""
    parents = {row.ppid for row in tree}
    leaves = [row.pid for row in tree if row.pid not in parents and row.pid != root]
    targets = leaves[:limit]
    if len(targets) < limit and any(row.pid == root for row in tree):
        targets.append(root)
    return targets


def _stamp(started: float) -> str:
    now = datetime.now(timezone.utc).strftime("%H:%M:%SZ")
    return f"[{now} +{time.monotonic() - started:7.1f}s]"


def _real_home() -> str:
    try:
        return pwd.getpwuid(os.getuid()).pw_dir
    except KeyError:
        return os.environ.get("HOME", "/")


Runner = Callable[[list[str], float, dict[str, str] | None], str]


def _run_diagnostic(args: list[str], timeout: float, env: dict[str, str] | None = None) -> str:
    try:
        result = subprocess.run(
            args,
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=timeout,
            env=env,
            check=False,
        )
    except FileNotFoundError:
        return f"{args[0]}: not available on this host\n"
    except subprocess.TimeoutExpired:
        return f"{args[0]}: no answer within {timeout:.0f}s\n"
    out = result.stdout
    if result.stderr:
        out += result.stderr
    if result.returncode != 0:
        out += f"({args[0]} exited {result.returncode})\n"
    return out


def capture_diagnostics(
    root: int,
    tail: Iterable[str],
    out_dir: Path,
    *,
    redact: Callable[[str], str],
    run: Runner = _run_diagnostic,
    elapsed: float = 0.0,
) -> Path:
    """Write the stall report for process group ``root`` and echo it to stdout."""
    out_dir.mkdir(parents=True, exist_ok=True)
    sections: list[tuple[str, str]] = []
    ps_out = run(
        ["ps", "-A", "-o", "pid=,ppid=,pgid=,stat=,etime=,%cpu=,command="],
        PS_TIMEOUT_SECS,
        None,
    )
    tree = build_tree(parse_ps(ps_out), root)
    header = "  PID  PPID  PGID STAT     ELAPSED %CPU COMMAND"
    sections.append(("process tree", "\n".join([header, *(row.line for row in tree)]) + "\n"))
    pids = [row.pid for row in tree]
    if pids:
        sections.append(("lsof", run(["lsof", "-n", "-P", "-p", ",".join(map(str, pids))],
                                     LSOF_TIMEOUT_SECS, None)))
    for pid in sample_targets(tree, root):
        # `sample` needs no sudo for the user's own processes; spindump does.
        sample_file = out_dir / f"sample-{pid}.txt"
        status = run(["sample", str(pid), str(SAMPLE_SECONDS), "-mayDie", "-file", str(sample_file)],
                     SAMPLE_TIMEOUT_SECS, None)
        text = ""
        with contextlib.suppress(OSError):
            text = sample_file.read_text(encoding="utf-8", errors="replace")
            sample_file.write_text(redact(text), encoding="utf-8")
        shown = "\n".join(text.splitlines()[:60])
        sections.append((f"sample {pid} (full: {sample_file.name})", f"{status}{shown}\n"))
    sections.append(("last output lines", "".join(tail)))
    security_env = dict(os.environ)
    security_env["HOME"] = _real_home()
    keychain = "".join(
        run(cmd, SECURITY_TIMEOUT_SECS, security_env)
        for cmd in (
            ["security", "default-keychain"],
            ["security", "list-keychains"],
            ["security", "show-keychain-info"],
        )
    )
    sections.append(("default keychain", keychain))

    report = [f"release build stalled: no exit after {elapsed:.0f}s; root pid {root}\n"]
    for title, body in sections:
        report.append(f"===== {title} =====\n{body}")
        if not body.endswith("\n"):
            report.append("\n")
    text = redact("".join(report))
    path = out_dir / "stall-report.txt"
    path.write_text(text, encoding="utf-8")
    print(text, end="", flush=True)
    return path


def _stop_group(proc: subprocess.Popen) -> None:
    for sig, wait in ((signal.SIGTERM, KILL_GRACE_SECS), (signal.SIGKILL, KILL_WAIT_SECS)):
        with contextlib.suppress(ProcessLookupError, PermissionError):
            os.killpg(proc.pid, sig)
        try:
            proc.wait(timeout=wait)
            return
        except subprocess.TimeoutExpired:
            continue


def run_bounded(
    args: list[str],
    *,
    cwd: Path,
    timeout: float,
    diagnostics_dir: Path,
    redact: Callable[[str], str] = lambda text: text,
    env: dict[str, str] | None = None,
    diagnose: Callable[..., Path] = capture_diagnostics,
) -> None:
    """Run ``args`` with closed stdin, timestamped output and a hard limit.

    Raises BuildTimedOut after writing diagnostics, or BuildFailed on a
    non-zero exit. The full output is kept in ``diagnostics_dir/cargo-build.log``.
    """
    diagnostics_dir.mkdir(parents=True, exist_ok=True)
    log_path = diagnostics_dir / "cargo-build.log"
    tail: collections.deque[str] = collections.deque(maxlen=TAIL_LINES)
    started = time.monotonic()
    shown = " ".join(redact(a) for a in args)
    print(f"{_stamp(started)} running (limit {timeout:.0f}s): {shown}", flush=True)
    proc = subprocess.Popen(
        args,
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        encoding="utf-8",
        errors="replace",
        start_new_session=True,
    )
    log = log_path.open("w", encoding="utf-8")

    def pump() -> None:
        assert proc.stdout is not None
        for line in proc.stdout:
            line = f"{_stamp(started)} {redact(line)}"
            tail.append(line)
            with contextlib.suppress(Exception):
                log.write(line)
                log.flush()
                sys.stdout.write(line)
                sys.stdout.flush()

    reader = threading.Thread(target=pump, daemon=True)
    reader.start()
    try:
        try:
            returncode = proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            elapsed = time.monotonic() - started
            try:
                report = diagnose(proc.pid, list(tail), diagnostics_dir, redact=redact, elapsed=elapsed)
            finally:
                _stop_group(proc)
            raise BuildTimedOut(
                f"release build exceeded {timeout:.0f}s and was killed; "
                f"diagnostics: {report} (set {TIMEOUT_ENV} to change the limit)"
            )
        except BaseException:
            _stop_group(proc)
            raise
    finally:
        reader.join(timeout=10)
        # A reader still blocked holds the pipe's lock; leave that pipe to it.
        if not reader.is_alive() and proc.stdout is not None:
            proc.stdout.close()
        with contextlib.suppress(Exception):
            log.close()
    elapsed = time.monotonic() - started
    print(f"{_stamp(started)} exited {returncode} after {elapsed:.0f}s", flush=True)
    if returncode != 0:
        raise BuildFailed(f"command failed ({returncode}): {shown}")
