#!/usr/bin/env python3
"""Host-owned transaction guardian for the M3 Sandbox canary.

The guardian, rather than the Actions shell, creates the machine-wide canary
lease.  Once it owns that lease it is responsible for restoring the exact
production daemon process even when the launching shell disappears.
"""

from __future__ import annotations

import argparse
import contextlib
import ctypes
import ctypes.util
import fcntl
import hashlib
import json
import os
import secrets
import signal
import socket
import stat
import struct
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, NoReturn, Optional, Union


class GuardianError(RuntimeError):
    """Fail-closed canary transaction error."""


class RetainedWriterDomain(GuardianError):
    """The production writer domain remained contended through an idle wait."""

    def __init__(
        self,
        holders: tuple[int, ...],
        *,
        continuously_contended: bool,
        continuous_production_ownership: bool,
    ) -> None:
        self.holders = holders
        self.continuously_contended = continuously_contended
        self.continuous_production_ownership = continuous_production_ownership
        super().__init__(
            "production daemon retained the writer-domain lock through the "
            f"bounded idle wait: {holders!r}"
        )


class OwnerEnded(RuntimeError):
    """The Actions owner exited or was cancelled before writing done."""


class ReconciledAfterOwnerEnded(RuntimeError):
    """A retained lease was repaired after its new Actions owner departed."""


class LeaseCreationCommitted(GuardianError):
    """Lease rename committed, but its parent durability check failed."""

    def __init__(
        self, message: str, metadata: os.stat_result, generation: str
    ) -> None:
        super().__init__(message)
        self.metadata = metadata
        self.generation = generation


LEGACY_TRANSITION = "legacy-lifetime-lock-quiesce-restore"
CORRECTED_TRANSITION = "corrected-idle-preserve-fence"
WRITER_DOMAIN_OVERLAP_EXIT_CODE = 75
WRITER_DOMAIN_OVERLAP_CLASSIFICATION = "sandbox_writer_domain_overlap"
PROTECTED_STDIO_PATH_ENV = "SHIPYARD_PROTECTED_STDIO_PATH"
LEGACY_LIFETIME_LOCK_VERSION = "0.108.1"
RETAINED_RECONCILIATION_REASON = "retained-lease-awaiting-idle"
RETAINED_RECONCILED_REASON = "retained-lease-reconciled"
RETAINED_RECONCILIATION_MAX_SECONDS = 6 * 60 * 60
OWNER_ENDED_BEFORE_COMPLETION = "Actions owner ended before canary completion"
PRESERVED_WORKER_OWNERSHIP_FAILURE = "preserved active worker ownership differs"
RETAINED_OWNER_ENDED_FAILURE = (
    f"GuardianError: OwnerEnded: {OWNER_ENDED_BEFORE_COMPLETION}; "
    f"restore production: GuardianError: {PRESERVED_WORKER_OWNERSHIP_FAILURE}; "
    f"restore production: GuardianError: {PRESERVED_WORKER_OWNERSHIP_FAILURE}"
)
LEASE_GENERATION_MARKER = ".shipyard-lease-generation.json"
LEASE_PHASE_ACQUIRING = "acquiring"
LEASE_PHASE_TRANSITIONING = "transitioning"


@dataclass(frozen=True)
class ProcessSnapshot:
    pid: int
    executable: str
    argv: tuple[str, ...]
    environment: dict[str, str]
    cwd: str
    stdin_path: str
    stdout_path: str
    stderr_path: str
    start_time: str

    @property
    def environment_sha256(self) -> str:
        encoded = json.dumps(
            sorted(self.environment.items()), separators=(",", ":"), ensure_ascii=True
        ).encode("utf-8")
        return hashlib.sha256(encoded).hexdigest()

    @property
    def argv_sha256(self) -> str:
        encoded = json.dumps(self.argv, separators=(",", ":")).encode("utf-8")
        return hashlib.sha256(encoded).hexdigest()


def _atomic_json(path: Path, payload: dict[str, object]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    temporary.write_text(json.dumps(payload, sort_keys=True) + "\n", encoding="utf-8")
    temporary.chmod(0o600)
    os.replace(temporary, path)


def _durable_atomic_json(path: Path, payload: dict[str, object]) -> None:
    """Atomically publish recovery authority and fsync bytes plus directory."""
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.{os.getpid()}.{time.time_ns()}.tmp")
    descriptor = os.open(
        temporary,
        os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
        0o600,
    )
    try:
        encoded = (json.dumps(payload, sort_keys=True) + "\n").encode("utf-8")
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(encoded)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    except Exception:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass
        raise


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _run(
    argv: list[str],
    *,
    cwd: Union[str, Path],
    env: dict[str, str],
    timeout: float = 15.0,
    check: bool = True,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        argv,
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        timeout=timeout,
        check=False,
    )
    if check and result.returncode != 0:
        detail = (result.stderr or result.stdout).strip()
        raise GuardianError(f"command failed ({result.returncode}): {argv!r}: {detail}")
    return result


def _capture_status_timeout(
    root: Path, child_pid: int, production_pid: int
) -> Path:
    """Preserve live macOS process/socket evidence before killing a stuck probe."""
    evidence = root / f"status-timeout-{int(time.time())}-{child_pid}"
    evidence.mkdir(parents=True, exist_ok=False)
    commands = {
        "child-sample.txt": ["/usr/bin/sample", str(child_pid), "1", "1"],
        "production-sample.txt": ["/usr/bin/sample", str(production_pid), "1", "1"],
        "child-lsof.txt": ["/usr/sbin/lsof", "-nP", "-p", str(child_pid)],
        "production-lsof.txt": ["/usr/sbin/lsof", "-nP", "-p", str(production_pid)],
        "unix-sockets.txt": ["/usr/sbin/netstat", "-anv", "-f", "unix"],
    }
    for name, argv in commands.items():
        try:
            result = subprocess.run(
                argv,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=4.0,
                check=False,
            )
            (evidence / name).write_bytes(result.stdout + result.stderr)
        except Exception as error:  # diagnostic failure must not mask the timeout
            (evidence / name).write_text(
                f"diagnostic failed: {type(error).__name__}: {error}\n",
                encoding="utf-8",
            )
    return evidence


def _run_status_probe(
    argv: list[str],
    *,
    cwd: Union[str, Path],
    env: dict[str, str],
    timeout: float,
    diagnostic_root: Path,
    production_pid: int,
) -> subprocess.CompletedProcess[str]:
    """Run status while retaining the live child long enough to diagnose a timeout."""
    process = subprocess.Popen(
        argv,
        cwd=cwd,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        try:
            evidence_detail = str(
                _capture_status_timeout(
                    diagnostic_root, process.pid, production_pid
                )
            )
        except Exception as diagnostic_error:
            evidence_detail = (
                "diagnostic capture failed: "
                f"{type(diagnostic_error).__name__}: {diagnostic_error}"
            )
        process.terminate()
        try:
            stdout, stderr = process.communicate(timeout=2.0)
        except subprocess.TimeoutExpired:
            process.kill()
            stdout, stderr = process.communicate(timeout=2.0)
        raise subprocess.TimeoutExpired(
            argv,
            timeout,
            output=stdout or error.output,
            stderr=(stderr or error.stderr or "")
            + f"\ntimeout diagnostics: {evidence_detail}",
        ) from error
    return subprocess.CompletedProcess(argv, process.returncode, stdout, stderr)


def _darwin_argv_environment(pid: int) -> tuple[str, tuple[str, ...], dict[str, str]]:
    if sys.platform != "darwin":
        raise GuardianError("the production process snapshot is macOS-only")
    libc_path = ctypes.util.find_library("c")
    if not libc_path:
        raise GuardianError("could not locate libc for KERN_PROCARGS2")
    libc = ctypes.CDLL(libc_path, use_errno=True)
    mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2, pid
    size = ctypes.c_size_t(0)
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0) != 0:
        raise OSError(ctypes.get_errno(), "KERN_PROCARGS2 size failed")
    buffer = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buffer, ctypes.byref(size), None, 0) != 0:
        raise OSError(ctypes.get_errno(), "KERN_PROCARGS2 read failed")
    data = buffer.raw[: size.value]
    if len(data) < 4:
        raise GuardianError("KERN_PROCARGS2 returned a truncated record")
    argc = struct.unpack_from("i", data)[0]
    position = 4
    executable_end = data.find(b"\0", position)
    if executable_end < 0:
        raise GuardianError("KERN_PROCARGS2 omitted the executable terminator")
    executable = os.fsdecode(data[position:executable_end])
    position = executable_end
    while position < len(data) and data[position] == 0:
        position += 1
    strings: list[str] = []
    while position < len(data):
        end = data.find(b"\0", position)
        if end <= position:
            break
        strings.append(os.fsdecode(data[position:end]))
        position = end + 1
    if argc <= 0 or len(strings) < argc:
        raise GuardianError("KERN_PROCARGS2 returned incomplete argv")
    argv = tuple(strings[:argc])
    environment: dict[str, str] = {}
    for entry in strings[argc:]:
        if "=" in entry:
            name, value = entry.split("=", 1)
            environment[name] = value
    if not environment:
        raise GuardianError("KERN_PROCARGS2 returned no environment")
    return executable, argv, environment


def _lsof_field(
    pid: int, descriptor: str, *, deadline: Optional[float] = None
) -> str:
    result = _run(
        ["/usr/sbin/lsof", "-a", "-p", str(pid), "-d", descriptor, "-Fn"],
        cwd="/",
        env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin"},
        timeout=_bounded_timeout(deadline),
    )
    values = [line[1:] for line in result.stdout.splitlines() if line.startswith("n")]
    if len(values) != 1:
        raise GuardianError(f"could not resolve fd {descriptor} for pid {pid}: {values!r}")
    return values[0]


def snapshot_process(pid: int, *, deadline: Optional[float] = None) -> ProcessSnapshot:
    executable, argv, environment = _darwin_argv_environment(pid)
    start_time = _process_start(pid, deadline=deadline)
    if start_time is None:
        raise GuardianError(f"process {pid} disappeared while being snapshotted")
    return ProcessSnapshot(
        pid=pid,
        executable=executable,
        argv=argv,
        environment=environment,
        cwd=_lsof_field(pid, "cwd", deadline=deadline),
        stdin_path=_lsof_field(pid, "0", deadline=deadline),
        stdout_path=_lsof_field(pid, "1", deadline=deadline),
        stderr_path=_lsof_field(pid, "2", deadline=deadline),
        start_time=start_time,
    )


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def _json_object(path: Path) -> dict[str, object]:
    metadata = path.lstat()
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_uid != os.getuid()
        or metadata.st_nlink != 1
    ):
        raise GuardianError(f"retained-lease evidence has unsafe metadata: {path}")
    try:
        descriptor = os.open(path, os.O_RDONLY | os.O_NOFOLLOW)
        with os.fdopen(descriptor, "r", encoding="utf-8") as handle:
            opened = os.fstat(handle.fileno())
            if (opened.st_dev, opened.st_ino) != (metadata.st_dev, metadata.st_ino):
                raise GuardianError(
                    f"retained-lease evidence changed while opening: {path}"
                )
            payload = handle.read(1024 * 1024 + 1)
        if len(payload) > 1024 * 1024:
            raise GuardianError(f"retained-lease evidence is oversized: {path}")
        value = json.loads(payload)
    except (OSError, json.JSONDecodeError) as error:
        raise GuardianError(f"could not read retained-lease evidence {path}: {error}") from error
    if not isinstance(value, dict):
        raise GuardianError(f"retained-lease evidence is not an object: {path}")
    return value


def _validate_private_directory(path: Path) -> os.stat_result:
    metadata = path.lstat()
    if (
        not stat.S_ISDIR(metadata.st_mode)
        or stat.S_ISLNK(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o700
        or metadata.st_uid != os.getuid()
    ):
        raise GuardianError(f"retained-lease directory has unsafe metadata: {path}")
    return metadata


def _lease_generation_state(path: Path) -> tuple[os.stat_result, str, str]:
    """Authenticate one private lease directory and its random generation."""
    metadata = _validate_private_directory(path)
    entries = tuple(path.iterdir())
    marker = path / LEASE_GENERATION_MARKER
    if entries != (marker,):
        raise GuardianError("retained lease has unexpected generation contents")
    payload = _json_object(marker)
    generation = payload.get("generation")
    phase = payload.get("phase")
    if (
        payload.get("schema_version") != 1
        or not isinstance(generation, str)
        or len(generation) != 64
        or any(character not in "0123456789abcdef" for character in generation)
        or phase not in (LEASE_PHASE_ACQUIRING, LEASE_PHASE_TRANSITIONING)
    ):
        raise GuardianError("retained lease generation marker is invalid")
    return metadata, generation, phase


def _validate_lease_generation(path: Path) -> tuple[os.stat_result, str]:
    metadata, generation, _ = _lease_generation_state(path)
    return metadata, generation


def _create_lease_generation(path: Path) -> tuple[os.stat_result, str]:
    generation = secrets.token_hex(32)
    staging = path.with_name(f".{path.name}.creating-{generation}")
    if staging.exists() or staging.is_symlink():
        raise GuardianError("lease generation staging path already exists")
    os.mkdir(staging, 0o700)
    committed = False
    staged_metadata = _validate_private_directory(staging)
    try:
        _durable_atomic_json(
            staging / LEASE_GENERATION_MARKER,
            {
                "schema_version": 1,
                "generation": generation,
                "phase": LEASE_PHASE_ACQUIRING,
            },
        )
        staged_metadata, observed, phase = _lease_generation_state(staging)
        if observed != generation or phase != LEASE_PHASE_ACQUIRING:
            raise GuardianError("new lease generation marker changed during creation")
        if path.exists() or path.is_symlink():
            raise GuardianError("host lease appeared during generation preparation")
        os.rename(staging, path)
        committed = True
        parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)
        metadata, observed, phase = _lease_generation_state(path)
        if (
            (metadata.st_dev, metadata.st_ino)
            != (staged_metadata.st_dev, staged_metadata.st_ino)
            or observed != generation
            or phase != LEASE_PHASE_ACQUIRING
        ):
            raise GuardianError("committed lease generation changed during creation")
        return metadata, generation
    except Exception as error:
        if committed:
            try:
                metadata, observed, _ = _lease_generation_state(path)
            except Exception:
                raise
            if (
                (metadata.st_dev, metadata.st_ino)
                != (staged_metadata.st_dev, staged_metadata.st_ino)
                or observed != generation
            ):
                raise GuardianError(
                    "committed lease identity changed after creation failure"
                ) from error
            raise LeaseCreationCommitted(
                f"lease creation committed before durability failure: {error}",
                metadata,
                generation,
            ) from error
        metadata = _validate_private_directory(staging)
        if (metadata.st_dev, metadata.st_ino) != (
            staged_metadata.st_dev,
            staged_metadata.st_ino,
        ):
            raise GuardianError("uncommitted lease staging identity changed") from error
        entries = tuple(staging.iterdir())
        marker = staging / LEASE_GENERATION_MARKER
        if entries == (marker,):
            _, observed = _validate_lease_generation(staging)
            if observed != generation:
                raise GuardianError("uncommitted lease staging generation changed") from error
            marker.unlink()
        elif entries:
            raise GuardianError("uncommitted lease staging contents changed") from error
        os.rmdir(staging)
        raise


def _advance_lease_generation(
    path: Path,
    expected_identity: tuple[int, int, int],
    expected_generation: str,
) -> os.stat_result:
    metadata, generation, phase = _lease_generation_state(path)
    if (metadata.st_dev, metadata.st_ino, metadata.st_ctime_ns) != expected_identity:
        raise GuardianError("lease identity changed before transition")
    if generation != expected_generation or phase != LEASE_PHASE_ACQUIRING:
        raise GuardianError("lease generation cannot enter transition")
    _durable_atomic_json(
        path / LEASE_GENERATION_MARKER,
        {
            "schema_version": 1,
            "generation": generation,
            "phase": LEASE_PHASE_TRANSITIONING,
        },
    )
    advanced, observed, observed_phase = _lease_generation_state(path)
    if (
        (advanced.st_dev, advanced.st_ino) != (metadata.st_dev, metadata.st_ino)
        or observed != generation
        or observed_phase != LEASE_PHASE_TRANSITIONING
    ):
        raise GuardianError("lease generation changed while entering transition")
    return advanced


def _remove_generation_bound_lease(
    path: Path,
    expected_identity: tuple[int, int, int],
    expected_generation: str,
) -> None:
    """Atomically detach a generation before deleting its private marker."""
    metadata, generation = _validate_lease_generation(path)
    if (metadata.st_dev, metadata.st_ino, metadata.st_ctime_ns) != expected_identity:
        raise GuardianError("refusing to release a replaced host lease")
    if generation != expected_generation:
        raise GuardianError("refusing to release a different host lease generation")
    tombstone = path.with_name(f".{path.name}.removed-{generation}")
    if tombstone.exists() or tombstone.is_symlink():
        raise GuardianError("host lease removal tombstone already exists")
    os.rename(path, tombstone)
    parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(parent)
    finally:
        os.close(parent)
    marker = tombstone / LEASE_GENERATION_MARKER
    moved_metadata, moved_generation = _validate_lease_generation(tombstone)
    # The rename itself changes directory ctime on supported hosts.  Device and
    # inode must survive the detach; the random generation authenticates that
    # it is the same lease rather than an inode-reuse collision.
    if (moved_metadata.st_dev, moved_metadata.st_ino) != expected_identity[:2]:
        raise GuardianError("detached host lease identity changed")
    if moved_generation != expected_generation:
        raise GuardianError("detached host lease generation changed")
    marker.unlink()
    os.rmdir(tombstone)
    parent = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
    try:
        os.fsync(parent)
    finally:
        os.close(parent)


def _open_verified_private_lock(path: Path):
    try:
        descriptor = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    except OSError as error:
        raise GuardianError(f"could not safely open lock {path}: {error}") from error
    handle = os.fdopen(descriptor, "a+b")
    metadata = os.fstat(handle.fileno())
    if (
        not stat.S_ISREG(metadata.st_mode)
        or stat.S_IMODE(metadata.st_mode) != 0o600
        or metadata.st_uid != os.getuid()
        or metadata.st_nlink != 1
    ):
        handle.close()
        raise GuardianError(f"lock metadata is unsafe: {path}")
    _validate_open_lock_path(path, metadata)
    return handle


def _validate_open_lock_path(path: Path, opened: os.stat_result) -> None:
    current = path.lstat()
    if (
        stat.S_ISLNK(current.st_mode)
        or not stat.S_ISREG(current.st_mode)
        or (current.st_dev, current.st_ino) != (opened.st_dev, opened.st_ino)
    ):
        raise GuardianError(f"lock identity changed: {path}")


def _argument_value(argv: tuple[str, ...], name: str) -> Optional[str]:
    for index, argument in enumerate(argv[:-1]):
        if argument == name:
            return argv[index + 1]
    return None


def _is_guardian_argv_for_lease(argv: tuple[str, ...], lease_dir: Path) -> bool:
    script_positions = argv[:2]
    return (
        any(Path(argument).name == "sandbox-daemon-guardian.py" for argument in script_positions)
        and _argument_value(argv, "--lease-dir") == str(lease_dir)
    )


def _live_guardians_for_lease(lease_dir: Path) -> tuple[int, ...]:
    """Return exact live guardian argv owners, excluding this process."""
    result = subprocess.run(
        ["/usr/bin/pgrep", "-f", "sandbox-daemon-guardian.py"],
        capture_output=True,
        text=True,
        timeout=5.0,
        check=False,
    )
    if result.returncode not in (0, 1) or result.stderr.strip():
        raise GuardianError(
            "could not inspect retained-lease guardians: "
            f"returncode={result.returncode}, stderr={result.stderr.strip()!r}"
        )
    owners: list[int] = []
    for line in result.stdout.splitlines():
        if not line.isdigit():
            raise GuardianError(f"invalid guardian pid result: {line!r}")
        pid = int(line)
        if pid == os.getpid():
            continue
        try:
            _, argv, _ = _darwin_argv_environment(pid)
        except (OSError, GuardianError):
            if _pid_alive(pid):
                raise GuardianError(f"could not authenticate possible guardian pid {pid}")
            continue
        if _is_guardian_argv_for_lease(argv, lease_dir):
            owners.append(pid)
    return tuple(sorted(owners))


def _bounded_timeout(deadline: Optional[float], default: float = 15.0) -> float:
    if deadline is None:
        return default
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise GuardianError("bounded production verification deadline expired")
    return min(default, remaining)


LOCK_HOLDER_ATTEMPT_TIMEOUT = 7.0
LOCK_HOLDER_TOTAL_TIMEOUT = 15.0
LOCK_HOLDER_KERNEL_TIMEOUT = "2"


def _process_start(pid: int, *, deadline: Optional[float] = None) -> Optional[str]:
    result = subprocess.run(
        ["/bin/ps", "-p", str(pid), "-o", "lstart="],
        capture_output=True,
        text=True,
        timeout=_bounded_timeout(deadline),
        check=False,
    )
    value = result.stdout.strip()
    return value or None


def _lock_holders(
    path: Path,
    *,
    deadline: Optional[float] = None,
    retry_after_timeout: Optional[Callable[[float], object]] = None,
    diagnostic_root: Optional[Path] = None,
) -> tuple[int, ...]:
    argv = [
        "/usr/sbin/lsof",
        "-nP",
        "-S",
        LOCK_HOLDER_KERNEL_TIMEOUT,
        "-F",
        "pf",
        "--",
        str(path),
    ]
    operation_deadline = deadline or (time.monotonic() + LOCK_HOLDER_TOTAL_TIMEOUT)
    attempts = 2 if retry_after_timeout is not None else 1
    result: Optional[subprocess.CompletedProcess[str]] = None
    for attempt in range(1, attempts + 1):
        timeout = _bounded_timeout(
            operation_deadline, default=LOCK_HOLDER_ATTEMPT_TIMEOUT
        )
        try:
            result = subprocess.run(
                argv,
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
            )
            break
        except subprocess.TimeoutExpired as error:
            if diagnostic_root is not None:
                try:
                    lock_stat = path.stat()
                    _atomic_json(
                        diagnostic_root
                        / f"lock-holder-timeout-{time.time_ns()}-{attempt}.json",
                        {
                            "schema_version": 1,
                            "argv": argv,
                            "attempt": attempt,
                            "timeout_seconds": timeout,
                            "partial_stdout": error.stdout.decode(errors="replace")
                            if isinstance(error.stdout, bytes)
                            else (error.stdout or ""),
                            "partial_stderr": error.stderr.decode(errors="replace")
                            if isinstance(error.stderr, bytes)
                            else (error.stderr or ""),
                            "lock_device": lock_stat.st_dev,
                            "lock_inode": lock_stat.st_ino,
                            "lock_mode": lock_stat.st_mode,
                            "lock_contended_after_timeout": (
                                _exclusive_lock_is_contended(path)
                            ),
                        },
                    )
                except Exception:
                    # Diagnostic capture must never replace the authoritative
                    # timeout or exact-identity revalidation outcome.
                    pass
            if attempt == attempts:
                raise GuardianError(
                    f"timed out inspecting writer-domain holders for {path} "
                    f"after {attempt} attempt(s)"
                ) from error
            # A second observation is safe only after the exact production
            # process, start time, installed binary, argv, environment, and
            # repository/worker authority have all been revalidated.
            assert retry_after_timeout is not None
            retry_after_timeout(operation_deadline)

    assert result is not None
    diagnostic = result.stderr.strip()
    if result.returncode not in (0, 1) or diagnostic:
        raise GuardianError(
            f"could not inspect writer-domain holders for {path}: "
            f"returncode={result.returncode}, stderr={diagnostic!r}"
        )
    holders: list[int] = []
    current_pid: Optional[int] = None
    current_has_file = False
    for field in result.stdout.splitlines():
        if field.startswith("p") and field[1:].isdigit():
            if current_pid is not None and not current_has_file:
                raise GuardianError(
                    f"incomplete lsof holder result for {path}: pid {current_pid} "
                    "has no file record"
                )
            current_pid = int(field[1:])
            current_has_file = False
            holders.append(current_pid)
        elif field.startswith("f") and len(field) > 1 and current_pid is not None:
            current_has_file = True
        else:
            raise GuardianError(
                f"invalid structured lsof field for {path}: {field!r}"
            )
    if current_pid is not None and not current_has_file:
        raise GuardianError(
            f"incomplete lsof holder result for {path}: pid {current_pid} "
            "has no file record"
        )
    observed = tuple(sorted(set(holders)))
    if (result.returncode == 0) != bool(observed):
        raise GuardianError(
            f"inconsistent lsof holder result for {path}: "
            f"returncode={result.returncode}, holders={observed!r}"
        )
    return observed


def _exclusive_lock_is_contended(path: Path) -> bool:
    with path.open("a+b") as handle:
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        return False


def _wait_for_idle_writer_domain(
    path: Path,
    production_pid: int,
    *,
    timeout: float = 10.0,
    poll_interval: float = 0.1,
    stable_observations: int = 3,
    initial_holders: Optional[tuple[int, ...]] = None,
    verify_production: Optional[Callable[[Optional[float]], object]] = None,
    diagnostic_root: Optional[Path] = None,
) -> None:
    """Distinguish a transient production mutation from a lifetime lock.

    Corrected daemons acquire the writer-domain lock only around mutations, so
    finalization can legitimately race a short critical section.  A pre-cutover
    daemon's lifetime lock never opens a lock-free observation window.  Foreign
    descriptors remain an immediate ownership failure rather than being hidden
    by the retry window.
    """
    deadline = time.monotonic() + timeout
    stable = 0
    last_holders: tuple[int, ...] = ()
    continuously_contended = True
    continuous_production_ownership = (
        initial_holders == (production_pid,) if initial_holders is not None else True
    )

    def fail_if_expired() -> None:
        if time.monotonic() >= deadline:
            raise RetainedWriterDomain(
                last_holders,
                continuously_contended=continuously_contended,
                continuous_production_ownership=continuous_production_ownership,
            )

    while True:
        fail_if_expired()
        if verify_production is not None:
            verify_production(deadline)
        fail_if_expired()
        holders = _lock_holders(
            path,
            deadline=deadline,
            retry_after_timeout=(
                (lambda retry_deadline: verify_production(retry_deadline))
                if verify_production is not None
                else None
            ),
            diagnostic_root=diagnostic_root,
        )
        last_holders = holders
        if holders != (production_pid,):
            continuous_production_ownership = False
        foreign_holders = tuple(pid for pid in holders if pid != production_pid)
        if foreign_holders:
            raise GuardianError(
                "foreign process entered the production writer domain: "
                f"{foreign_holders!r}"
            )
        if not _exclusive_lock_is_contended(path):
            continuously_contended = False
            stable += 1
            if stable >= stable_observations:
                if verify_production is not None:
                    verify_production(deadline)
                fail_if_expired()
                return
        else:
            stable = 0
        fail_if_expired()
        time.sleep(poll_interval)


def _select_transition(
    production_pid: int, holders: tuple[int, ...], contended: bool
) -> str:
    if holders == (production_pid,) and contended:
        return LEGACY_TRANSITION
    # The corrected daemon may retain an open descriptor for mutation-scoped
    # use without retaining an advisory lock. lsof reports that descriptor as
    # a holder, so contention—not descriptor presence—is the lock authority.
    if holders in ((), (production_pid,)) and not contended:
        return CORRECTED_TRANSITION
    raise GuardianError(
        "ambiguous production writer-domain state: "
        f"pid={production_pid}, holders={holders!r}, contended={contended}"
    )


def _select_transition_after_bounded_observation(
    path: Path,
    production_pid: int,
    holders: tuple[int, ...],
    contended: bool,
    *,
    verify_production: Optional[Callable[[Optional[float]], object]] = None,
    installed_version: Optional[Callable[[Optional[float]], str]] = None,
    diagnostic_root: Optional[Path] = None,
) -> str:
    """Classify a contended production-only lock without guessing its lifetime.

    A corrected daemon can momentarily look exactly like a legacy daemon while
    it persists one mutation.  Stable idle observations prove the corrected
    path; continuous, exact production ownership through the bounded window
    proves the compatibility quiesce/restore path.
    """
    if not contended or holders not in ((), (production_pid,)):
        return _select_transition(production_pid, holders, contended)
    try:
        _wait_for_idle_writer_domain(
            path,
            production_pid,
            initial_holders=holders,
            verify_production=verify_production,
            diagnostic_root=diagnostic_root,
        )
    except RetainedWriterDomain as error:
        if (
            error.holders != (production_pid,)
            or not error.continuously_contended
            or not error.continuous_production_ownership
        ):
            raise GuardianError(
                "ambiguous production writer-domain state after bounded observation: "
                f"pid={production_pid}, holders={error.holders!r}, "
                f"continuously_contended={error.continuously_contended}, "
                "continuous_production_ownership="
                f"{error.continuous_production_ownership}"
            ) from error
        if installed_version is None:
            raise GuardianError(
                "persistent production writer-domain ownership lacks trusted "
                "installed-version evidence"
            ) from error
        final_deadline = time.monotonic() + 15.0
        version = installed_version(final_deadline)
        if version != LEGACY_LIFETIME_LOCK_VERSION:
            raise GuardianError(
                "persistent production writer-domain ownership is not a known "
                f"legacy lifetime-lock build: version={version!r}"
            ) from error
        if verify_production is not None:
            verify_production(final_deadline)
        final_holders = _lock_holders(
            path,
            deadline=final_deadline,
            retry_after_timeout=verify_production,
            diagnostic_root=diagnostic_root,
        )
        final_contended = _exclusive_lock_is_contended(path)
        if verify_production is not None:
            verify_production(final_deadline)
        _bounded_timeout(final_deadline)
        if final_holders == (production_pid,) and final_contended:
            return LEGACY_TRANSITION
        raise GuardianError(
            "ambiguous production writer-domain state after bounded observation: "
            f"pid={production_pid}, holders={final_holders!r}, "
            f"contended={final_contended}"
        ) from error
    return CORRECTED_TRANSITION


def _running_daemon_version(
    snapshot: ProcessSnapshot,
    installed: Path,
    state_dir: Path,
    deadline: Optional[float] = None,
) -> str:
    status = _peer_verified_daemon_status(
        state_dir, snapshot.pid, deadline=deadline
    )
    version = status.get("shipyard_version")
    if status.get("running") is not True or not isinstance(version, str) or not version:
        raise GuardianError("production daemon did not report a trusted running version")
    installed_version = _installed_cli_version(
        snapshot, installed, deadline=deadline
    )
    if installed_version != version:
        raise GuardianError(
            "running and installed Shipyard versions differ: "
            f"running={version!r}, installed={installed_version!r}"
        )
    return version


def _installed_cli_version(
    snapshot: ProcessSnapshot,
    installed: Path,
    *,
    deadline: Optional[float] = None,
) -> str:
    environment = dict(snapshot.environment)
    environment.pop(PROTECTED_STDIO_PATH_ENV, None)
    result = _run(
        [str(installed), "--version"],
        cwd="/",
        env=environment,
        timeout=_bounded_timeout(deadline),
    )
    prefix = "shipyard "
    output = result.stdout.strip()
    if not output.startswith(prefix) or not output[len(prefix) :]:
        raise GuardianError(f"installed Shipyard returned invalid version: {output!r}")
    return output[len(prefix) :]


def _repo_args(argv: tuple[str, ...]) -> tuple[str, ...]:
    repos: list[str] = []
    for index, argument in enumerate(argv[:-1]):
        if argument == "--repo":
            repos.append(argv[index + 1])
    return tuple(sorted(set(repos)))


def _mode_arg(argv: tuple[str, ...]) -> Optional[str]:
    for index, argument in enumerate(argv[:-1]):
        if argument == "--mode":
            return argv[index + 1]
    return None


def _json_command(
    snapshot: ProcessSnapshot,
    installed: Path,
    *args: str,
    deadline: Optional[float] = None,
    diagnostic_root: Optional[Path] = None,
) -> dict[str, object]:
    foreground_environment = dict(snapshot.environment)
    # This foreground verifier writes to a captured pipe, not the daemon log.
    # Inheriting the daemon's marker can make a read-only status probe wait
    # behind the very writer-domain audit the guardian is measuring.
    foreground_environment.pop(PROTECTED_STDIO_PATH_ENV, None)
    # These probes read only explicit production state/IPC. Never inherit the
    # daemon's checkout cwd: an external volume can be temporarily unavailable,
    # and worker status does not need repository-local configuration here.
    probe_cwd = "/"
    argv = [str(installed), "--cwd", probe_cwd, "--json", *args]
    timeout = _bounded_timeout(deadline)
    # Deadline-bound retry revalidation must not enter the richer timeout
    # diagnostics below: those intentionally run several independent probes
    # and could outlive the single aggregate holder-observation deadline.
    if diagnostic_root is None or deadline is not None:
        result = _run(
            argv,
            cwd=probe_cwd,
            env=foreground_environment,
            timeout=timeout,
        )
    else:
        result = _run_status_probe(
            argv,
            cwd=probe_cwd,
            env=foreground_environment,
            timeout=timeout,
            diagnostic_root=diagnostic_root,
            production_pid=snapshot.pid,
        )
    value = json.loads(result.stdout)
    if not isinstance(value, dict):
        raise GuardianError(f"expected JSON object from {args!r}")
    return value


def _peer_verified_daemon_status(
    state_dir: Path,
    expected_pid: int,
    *,
    deadline: Optional[float] = None,
) -> dict[str, object]:
    """Read status over the same Unix connection whose peer PID is verified."""
    operation_deadline = deadline if deadline is not None else time.monotonic() + 15.0
    socket_path = state_dir / "daemon" / "daemon.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
        connection.settimeout(_bounded_timeout(operation_deadline))
        connection.connect(str(socket_path))
        peer_pid = connection.getsockopt(0, 2)  # macOS SOL_LOCAL / LOCAL_PEERPID
        if peer_pid != expected_pid:
            raise GuardianError(
                f"daemon socket peer pid differs: expected={expected_pid}, actual={peer_pid}"
            )
        connection.settimeout(_bounded_timeout(operation_deadline))
        connection.sendall(b'{"type":"status"}\n')
        buffered = b""
        while True:
            connection.settimeout(_bounded_timeout(operation_deadline))
            chunk = connection.recv(65536)
            if not chunk:
                raise GuardianError("daemon socket closed before a status frame")
            buffered += chunk
            if len(buffered) > 1024 * 1024:
                raise GuardianError("daemon status response exceeded one MiB")
            while b"\n" in buffered:
                raw_line, buffered = buffered.split(b"\n", 1)
                try:
                    frame = json.loads(raw_line)
                except (UnicodeDecodeError, json.JSONDecodeError) as error:
                    raise GuardianError("daemon socket returned invalid JSON") from error
                if not isinstance(frame, dict):
                    raise GuardianError("daemon socket returned a non-object frame")
                frame_type = frame.get("type")
                if frame_type == "hello":
                    continue
                if frame_type != "status":
                    raise GuardianError(
                        f"daemon socket returned unexpected frame: {frame_type!r}"
                    )
                if connection.getsockopt(0, 2) != expected_pid:
                    raise GuardianError("daemon socket peer pid changed during status read")
                # A same-connection status frame is the daemon's readiness
                # proof. `running` is a CLI wrapper field, not part of the raw
                # v0.108.1-compatible IPC payload.
                return {**frame, "running": True}


def _active_runs(
    snapshot: ProcessSnapshot,
    installed: Path,
    *,
    state_dir: Path,
    deadline: Optional[float] = None,
    diagnostic_root: Optional[Path] = None,
) -> tuple[str, ...]:
    status = _json_command(
        snapshot,
        installed,
        "--mode",
        "shipyard",
        "--state-dir",
        str(state_dir),
        "status",
        deadline=deadline,
        diagnostic_root=diagnostic_root,
    )
    runs = status.get("active_runs")
    if not isinstance(runs, list):
        raise GuardianError("worker status active_runs is not an array")
    return tuple(sorted(str(run["id"]) for run in runs if isinstance(run, dict) and "id" in run))


def _configured_repos(
    snapshot: ProcessSnapshot,
    installed: Path,
    *,
    deadline: Optional[float] = None,
    diagnostic_root: Optional[Path] = None,
    state_dir: Optional[Path] = None,
) -> tuple[str, ...]:
    status = (
        _peer_verified_daemon_status(state_dir, snapshot.pid, deadline=deadline)
        if state_dir is not None
        else _json_command(
            snapshot,
            installed,
            "daemon",
            "status",
            deadline=deadline,
            diagnostic_root=diagnostic_root,
        )
    )
    configured = status.get("configured_repos")
    if isinstance(configured, list):
        return tuple(sorted(str(repo) for repo in configured))
    # v0.108.1 cannot report configured_repos. Its exact argv is the only
    # complete authority; registered_repos is merely the successful subset.
    return _repo_args(snapshot.argv)


def run_lifecycle(
    acquire: Callable[[], None],
    quiesce: Callable[[], None],
    start_candidate: Callable[[], None],
    wait_for_owner: Callable[[], None],
    stop_candidate: Callable[[], None],
    restore: Callable[[], None],
    release: Callable[[], None],
) -> None:
    """Small testable ordering kernel for cancellation/failure cleanup."""
    acquired = False
    quiesced = False
    candidate_started = False
    failure: Optional[Exception] = None
    try:
        acquire()
        acquired = True
        quiesce()
        quiesced = True
        start_candidate()
        candidate_started = True
        wait_for_owner()
    except Exception as error:
        failure = error

    cleanup_failures: list[str] = []
    restoration_failed = False
    for enabled, name, action in (
        (candidate_started, "stop candidate", stop_candidate),
        (quiesced, "restore production", restore),
        (acquired, "release lease", release),
    ):
        if not enabled:
            continue
        if name == "release lease" and restoration_failed:
            continue
        try:
            action()
        except Exception as error:
            cleanup_failures.append(f"{name}: {type(error).__name__}: {error}")
            if name == "restore production":
                restoration_failed = True

    if cleanup_failures:
        prefix = f"{type(failure).__name__}: {failure}; " if failure else ""
        raise GuardianError(prefix + "; ".join(cleanup_failures))
    if failure is not None:
        raise failure


class Guardian:
    def __init__(self, args: argparse.Namespace) -> None:
        self.args = args
        self.root = Path(args.canary_root)
        self.installed = Path(args.installed)
        self.candidate = Path(args.candidate)
        self.global_dir = Path(args.global_dir)
        self.state_dir = Path(args.state_dir)
        self.done_file = Path(args.done_file)
        self.ready_receipt = Path(args.ready_receipt)
        self.final_receipt = Path(args.final_receipt)
        self.lease_dir = Path(args.lease_dir)
        self.reconciliation_receipt = self.root / "retained-reconciliation.json"
        self.reconciliation_intent = self.root / "retained-reconciliation-intent.json"
        self.reconciliation_lock = self.lease_dir.with_name(
            f".{self.lease_dir.name}.reconcile.lock"
        )
        self.production_pid_file = Path(args.production_pid_file)
        self.production_state_dir = self.production_pid_file.parent.parent
        self.audit_ready_file = self.root / "exclusive-audit-ready"
        self.mutation_receipt = self.root / "mutation-fence.json"
        self.mutation_guard_path = (
            self.production_pid_file.parent.parent
            / f".sandbox-canary-guard-{self.root.name}"
        )
        self.mutation_probe_output = self.root / "unexpected-mutation-ran"
        self.snapshot: Optional[ProcessSnapshot] = None
        self.candidate_process: Optional[subprocess.Popen] = None
        self.restoration_process: Optional[subprocess.Popen] = None
        self.restored_pid: Optional[int] = None
        self.final_production_start_time: Optional[str] = None
        self.owner_start = _process_start(args.owner_pid)
        self.stop_requested = False
        self.lease_owned = False
        self.lease_device: Optional[int] = None
        self.lease_inode: Optional[int] = None
        self.lease_ctime_ns: Optional[int] = None
        self.lease_generation: Optional[str] = None
        self.production_stop_requested = False
        self.production_quiesced = False
        self.production_restored = False
        self.production_preserved = False
        self.production_identity_verified = False
        self.mutation_fence_proved = False
        self.candidate_stopped = True
        self.failure: Optional[str] = None
        self.reconciled_prior_canary_root: Optional[str] = None

    def request_stop(self, _signum: int, _frame: object) -> None:
        self.stop_requested = True

    @contextlib.contextmanager
    def retained_reconciliation_lock(self):
        self.reconciliation_lock.parent.mkdir(parents=True, exist_ok=True)
        try:
            descriptor = os.open(
                self.reconciliation_lock,
                os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW,
                0o600,
            )
        except OSError as error:
            raise GuardianError(
                f"could not safely open retained reconciliation lock: {error}"
            ) from error
        with os.fdopen(descriptor, "a+b") as handle:
            metadata = os.fstat(handle.fileno())
            if (
                not stat.S_ISREG(metadata.st_mode)
                or stat.S_IMODE(metadata.st_mode) != 0o600
                or metadata.st_uid != os.getuid()
                or metadata.st_nlink != 1
            ):
                raise GuardianError("retained reconciliation lock metadata is unsafe")
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
            try:
                path_metadata = self.reconciliation_lock.lstat()
                if (path_metadata.st_dev, path_metadata.st_ino) != (
                    metadata.st_dev,
                    metadata.st_ino,
                ):
                    raise GuardianError(
                        "retained reconciliation lock identity changed"
                    )
                yield
            finally:
                fcntl.flock(handle.fileno(), fcntl.LOCK_UN)

    def retained_legacy_evidence(self) -> tuple[Path, dict[str, object], dict[str, object]]:
        """Authenticate the one prior corrected guardian that retained this lease."""
        prefix = self.lease_dir.name
        if prefix.endswith("-lease"):
            prefix = prefix[: -len("-lease")]
        roots: list[Path] = []
        receipts: list[tuple[Path, dict[str, object]]] = []
        for root in sorted(self.lease_dir.parent.glob(f"{prefix}-[0-9]*-[0-9]*")):
            _validate_private_directory(root)
            roots.append(root)
            receipt_path = root / "guardian-receipt.json"
            if not receipt_path.is_file():
                continue
            receipt = _json_object(receipt_path)
            receipts.append((root, receipt))
        reconciled_roots = {
            receipt.get("reconciled_prior_canary_root")
            for _, receipt in receipts
            if receipt.get("reason") in ("completed", RETAINED_RECONCILED_REASON)
            and receipt.get("failure") is None
            and receipt.get("candidate_stopped") is True
            and receipt.get("production_identity_verified") is True
            and receipt.get("production_preserved") is True
            and receipt.get("lease_removed") is True
            and receipt.get("transition_path") == CORRECTED_TRANSITION
            and isinstance(receipt.get("reconciled_prior_canary_root"), str)
        }
        current_lease, current_generation = _validate_lease_generation(self.lease_dir)
        current_lease_identity = (
            current_lease.st_dev,
            current_lease.st_ino,
            current_lease.st_ctime_ns,
            current_generation,
        )
        prepared_intents: list[dict[str, object]] = []
        for root in roots:
            intent_path = root / "retained-reconciliation-intent.json"
            if not intent_path.is_file():
                continue
            intent = _json_object(intent_path)
            if (
                intent.get("schema_version") == 1
                and intent.get("transition_path") == CORRECTED_TRANSITION
                and intent.get("mutation_fence_proved") is True
                and isinstance(intent.get("prior_canary_root"), str)
                and isinstance(intent.get("lease_device"), int)
                and isinstance(intent.get("lease_inode"), int)
                and isinstance(intent.get("lease_ctime_ns"), int)
                and isinstance(intent.get("lease_generation"), str)
                and (
                    intent.get("lease_device"),
                    intent.get("lease_inode"),
                    intent.get("lease_ctime_ns"),
                    intent.get("lease_generation"),
                )
                != current_lease_identity
            ):
                prepared_intents.append(intent)
        retained = [
            (root, receipt)
            for root, receipt in receipts
            if receipt.get("lease_retained") is True
            and (
                receipt.get("lease_device"),
                receipt.get("lease_inode"),
                receipt.get("lease_ctime_ns"),
                receipt.get("lease_generation"),
            )
            == current_lease_identity
            and str(root) not in reconciled_roots
            and not any(
                intent.get("prior_canary_root") == str(root)
                and intent.get("old_production_pid")
                == receipt.get("old_production_pid")
                and intent.get("old_production_start_time")
                == receipt.get("old_production_start_time")
                and intent.get("installed_sha256")
                == receipt.get("installed_sha256")
                for intent in prepared_intents
            )
        ]
        if len(retained) != 1:
            raise GuardianError(
                "empty retained lease lacks one unambiguous prior receipt: "
                f"found={len(retained)}"
            )
        prior_root, receipt = retained[0]
        ready = _json_object(prior_root / "ready.json")
        mutation = _json_object(prior_root / "mutation-fence.json")
        failure = receipt.get("failure")
        allowed_worker_change_failure = (
            isinstance(failure, str)
            and "active workers changed during idle wait" in failure
            and PRESERVED_WORKER_OWNERSHIP_FAILURE in failure
        )
        allowed_owner_ended_failure = (
            failure == RETAINED_OWNER_ENDED_FAILURE
            and receipt.get("reason") == "failed"
            and receipt.get("production_preserved") is False
            and receipt.get("production_identity_verified") is False
        )
        allowed_failure = allowed_worker_change_failure or allowed_owner_ended_failure
        required = (
            receipt.get("schema_version") == 1,
            receipt.get("transition_path") == CORRECTED_TRANSITION,
            receipt.get("candidate_stopped") is True,
            receipt.get("production_quiesced") is False,
            receipt.get("production_restored") is False,
            receipt.get("mutation_fence_proved") is True,
            receipt.get("old_lifetime_lock_owned") is False,
            receipt.get("lease_removed") is False,
            receipt.get("active_runs") == [],
            ready.get("transition_path") == CORRECTED_TRANSITION,
            mutation.get("transition_path") == CORRECTED_TRANSITION,
            mutation.get("returncode") == WRITER_DOMAIN_OVERLAP_EXIT_CODE,
            mutation.get("overlap_classification")
            == WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
            mutation.get("mutation_absent") is True,
            mutation.get("selected_path_absent") is True,
            allowed_failure,
        )
        if not all(required):
            raise GuardianError("prior retained-lease receipt is not safely reconcilable")
        candidate_pid = ready.get("candidate_pid")
        if not isinstance(candidate_pid, int) or isinstance(candidate_pid, bool):
            raise GuardianError("prior retained-lease candidate pid is invalid")
        if _pid_alive(candidate_pid):
            raise GuardianError(f"prior candidate pid {candidate_pid} is still alive")
        for field in (
            "installed_sha256",
            "production_pid",
            "production_start_time",
        ):
            receipt_field = "old_production_pid" if field == "production_pid" else (
                "old_production_start_time" if field == "production_start_time" else field
            )
            if ready.get(field) != receipt.get(receipt_field):
                raise GuardianError(f"prior retained-lease {field} evidence disagrees")
        if ready.get("candidate_sha256") != receipt.get("candidate_sha256"):
            raise GuardianError("prior retained-lease candidate hash disagrees")
        if mutation.get("production_pid") != receipt.get("old_production_pid") or mutation.get(
            "production_start_time"
        ) != receipt.get("old_production_start_time"):
            raise GuardianError("prior retained-lease mutation identity disagrees")
        return prior_root, receipt, ready

    def snapshot_reconciliation_production(
        self, prior: dict[str, object]
    ) -> tuple[ProcessSnapshot, tuple[str, ...]]:
        pid_text = self.production_pid_file.read_text(encoding="utf-8").strip()
        snapshot = snapshot_process(int(pid_text))
        installed_hash = _sha256(self.installed)
        configured_repos = _configured_repos(
            snapshot,
            self.installed,
            diagnostic_root=self.root,
            state_dir=self.production_state_dir,
        )
        expected = {
            "old_production_pid": snapshot.pid,
            "old_production_start_time": snapshot.start_time,
            "installed_sha256": installed_hash,
            "argv_sha256": snapshot.argv_sha256,
            "environment_sha256": snapshot.environment_sha256,
            "cwd": snapshot.cwd,
            "mode": _mode_arg(snapshot.argv),
            "configured_repos": list(configured_repos),
        }
        for field, value in expected.items():
            if prior.get(field) != value:
                raise GuardianError(f"retained-lease production authority changed: {field}")
        if Path(snapshot.executable).resolve() != self.installed.resolve():
            raise GuardianError("retained-lease daemon executable changed")
        if configured_repos != _repo_args(snapshot.argv):
            raise GuardianError("retained-lease repository authority disagrees with argv")
        self.snapshot = snapshot
        self.installed_hash = installed_hash
        self.candidate_hash = _sha256(self.candidate)
        self.configured_repos = configured_repos
        self.worker_ids = ()
        self.lock_path = self.production_state_dir / ".sandbox-writer-domain.lock"
        self.transition_path = CORRECTED_TRANSITION
        self.old_lifetime_lock_owned = False
        self.mutation_fence_proved = True
        return snapshot, configured_repos

    def verify_reconciliation_production(self) -> tuple[str, ...]:
        snapshot = self.snapshot
        if snapshot is None:
            raise GuardianError("retained-lease production snapshot is unavailable")
        if _sha256(self.installed) != self.installed_hash:
            raise GuardianError("installed production binary changed during reconciliation")
        current_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
        if current_pid != snapshot.pid:
            raise GuardianError("production daemon pid changed during reconciliation")
        current = snapshot_process(current_pid)
        self.assert_process_identity(current, require_same_pid=True)
        repos = _configured_repos(
            current,
            self.installed,
            diagnostic_root=self.root,
            state_dir=self.production_state_dir,
        )
        if repos != self.configured_repos:
            raise GuardianError("configured repositories changed during reconciliation")
        return _active_runs(
            current,
            self.installed,
            state_dir=self.production_state_dir,
            diagnostic_root=self.root,
        )

    def write_reconciliation_receipt(
        self,
        *,
        reason: str,
        prior_root: Path,
        active_runs: tuple[str, ...],
        lease_device: int,
        lease_inode: int,
        lease_ctime_ns: int,
        lease_generation: str,
    ) -> None:
        snapshot = self.snapshot
        _durable_atomic_json(
            self.reconciliation_receipt,
            {
                "schema_version": 1,
                "reason": reason,
                "guardian_pid": os.getpid(),
                "guardian_start_time": _process_start(os.getpid()),
                "lease_dir": str(self.lease_dir),
                "lease_device": lease_device,
                "lease_inode": lease_inode,
                "lease_ctime_ns": lease_ctime_ns,
                "lease_generation": lease_generation,
                "prior_canary_root": str(prior_root),
                "candidate_stopped": True,
                "production_quiesced": False,
                "production_restored": False,
                "transition_path": CORRECTED_TRANSITION,
                "mutation_fence_proved": True,
                "old_production_pid": snapshot.pid if snapshot else None,
                "old_production_start_time": snapshot.start_time if snapshot else None,
                "installed_sha256": getattr(self, "installed_hash", None),
                "configured_repos": list(getattr(self, "configured_repos", ())),
                "active_runs": list(active_runs),
                "lease_removed": not self.lease_dir.exists(),
            },
        )

    @contextlib.contextmanager
    def final_reconciliation_writer_fence(self):
        """Exclude new production writers across final verification/removal."""
        turnstile_path = self.lock_path.with_name(
            ".sandbox-writer-domain.turnstile.lock"
        )
        deadline = time.monotonic() + 30.0
        with _open_verified_private_lock(turnstile_path) as turnstile, _open_verified_private_lock(
            self.lock_path
        ) as writer_domain:
            turnstile_stat = os.fstat(turnstile.fileno())
            writer_stat = os.fstat(writer_domain.fileno())
            while True:
                self.verify_reconciliation_production()
                try:
                    fcntl.flock(turnstile.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
                    break
                except BlockingIOError:
                    if time.monotonic() >= deadline:
                        raise GuardianError(
                            "timed out acquiring retained reconciliation turnstile"
                        )
                    time.sleep(0.05)
            try:
                while True:
                    self.verify_reconciliation_production()
                    try:
                        fcntl.flock(
                            writer_domain.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB
                        )
                        break
                    except BlockingIOError:
                        if time.monotonic() >= deadline:
                            raise GuardianError(
                                "timed out acquiring retained reconciliation writer domain"
                            )
                        time.sleep(0.05)
                try:
                    def validate_paths() -> None:
                        _validate_open_lock_path(turnstile_path, turnstile_stat)
                        _validate_open_lock_path(self.lock_path, writer_stat)

                    validate_paths()
                    yield validate_paths
                finally:
                    fcntl.flock(writer_domain.fileno(), fcntl.LOCK_UN)
            finally:
                fcntl.flock(turnstile.fileno(), fcntl.LOCK_UN)

    def reconcile_retained_lease(self) -> bool:
        """Reap one authenticated corrected-path lease after stable queue idle.

        Returns true when the current Actions owner is still alive and the
        caller should acquire a fresh lease for the canary.  A detached
        launchd guardian raises after terminal reconciliation instead.
        """
        lease_stat, lease_generation, lease_phase = _lease_generation_state(
            self.lease_dir
        )
        owners = _live_guardians_for_lease(self.lease_dir)
        if owners:
            raise GuardianError(f"retained lease still has a live guardian: {owners!r}")
        if lease_phase == LEASE_PHASE_ACQUIRING:
            _remove_generation_bound_lease(
                self.lease_dir,
                (lease_stat.st_dev, lease_stat.st_ino, lease_stat.st_ctime_ns),
                lease_generation,
            )
            return True
        prior_root, prior, _ = self.retained_legacy_evidence()
        self.reconciled_prior_canary_root = str(prior_root)
        self.snapshot_reconciliation_production(prior)
        deadline = time.monotonic() + RETAINED_RECONCILIATION_MAX_SECONDS
        delay = 1.0
        stable_idle = 0
        pending_written = False
        while time.monotonic() < deadline:
            if self.stop_requested:
                raise GuardianError("retained-lease reconciliation was interrupted")
            active_runs = self.verify_reconciliation_production()
            if active_runs:
                stable_idle = 0
                if not pending_written:
                    self.write_reconciliation_receipt(
                        reason=RETAINED_RECONCILIATION_REASON,
                        prior_root=prior_root,
                        active_runs=active_runs,
                        lease_device=lease_stat.st_dev,
                        lease_inode=lease_stat.st_ino,
                        lease_ctime_ns=lease_stat.st_ctime_ns,
                        lease_generation=lease_generation,
                    )
                    pending_written = True
            else:
                stable_idle += 1
                if stable_idle >= 3:
                    break
            time.sleep(delay)
            delay = min(delay * 2.0, 30.0)
        else:
            raise GuardianError("retained-lease production queue did not become idle")

        with self.final_reconciliation_writer_fence() as validate_lock_paths:
            if self.verify_reconciliation_production():
                raise GuardianError("production workers reappeared before lease removal")
            current_stat, current_generation = _validate_lease_generation(
                self.lease_dir
            )
            if (
                current_stat.st_dev,
                current_stat.st_ino,
                current_stat.st_ctime_ns,
                current_generation,
            ) != (
                lease_stat.st_dev,
                lease_stat.st_ino,
                lease_stat.st_ctime_ns,
                lease_generation,
            ):
                raise GuardianError("retained lease identity changed before removal")
            _durable_atomic_json(
                self.reconciliation_intent,
                {
                    "schema_version": 1,
                    "transition_path": CORRECTED_TRANSITION,
                    "mutation_fence_proved": True,
                    "prior_canary_root": str(prior_root),
                    "lease_device": lease_stat.st_dev,
                    "lease_inode": lease_stat.st_ino,
                    "lease_ctime_ns": lease_stat.st_ctime_ns,
                    "lease_generation": lease_generation,
                    "lease_tombstone": str(
                        self.lease_dir.with_name(
                            f".{self.lease_dir.name}.removed-{lease_generation}"
                        )
                    ),
                    "old_production_pid": self.snapshot.pid,
                    "old_production_start_time": self.snapshot.start_time,
                    "installed_sha256": self.installed_hash,
                },
            )
            validate_lock_paths()
            _remove_generation_bound_lease(
                self.lease_dir,
                (lease_stat.st_dev, lease_stat.st_ino, lease_stat.st_ctime_ns),
                lease_generation,
            )
        if pending_written:
            raise ReconciledAfterOwnerEnded(
                "retained lease reconciled after durable deferral"
            )
        if _process_start(self.args.owner_pid) != self.owner_start:
            raise ReconciledAfterOwnerEnded("retained lease reconciled after owner exit")
        return True

    def acquire(self) -> None:
        # Arm cleanup before the atomic mkdir.  Python signal handlers run only
        # between bytecodes, so after mkdir succeeds there is no bytecode where
        # the host lease exists but the guardian does not own its cleanup.
        with self.retained_reconciliation_lock():
            if self.lease_dir.exists():
                self.reconcile_retained_lease()
            self.lease_owned = True
            try:
                lease_stat, lease_generation = _create_lease_generation(self.lease_dir)
                self.lease_device = lease_stat.st_dev
                self.lease_inode = lease_stat.st_ino
                self.lease_ctime_ns = lease_stat.st_ctime_ns
                self.lease_generation = lease_generation
            except LeaseCreationCommitted as error:
                self.lease_device = error.metadata.st_dev
                self.lease_inode = error.metadata.st_ino
                self.lease_ctime_ns = error.metadata.st_ctime_ns
                self.lease_generation = error.generation
                raise
            except Exception:
                self.lease_owned = False
                self.lease_device = None
                self.lease_inode = None
                self.lease_ctime_ns = None
                self.lease_generation = None
                raise

    def preflight_and_transition(self) -> None:
        if self.owner_start is None:
            raise GuardianError("Actions owner was not alive when guardian started")
        pid_text = self.production_pid_file.read_text(encoding="utf-8").strip()
        snapshot = snapshot_process(int(pid_text))
        if Path(snapshot.executable).resolve() != self.installed.resolve():
            raise GuardianError("production daemon executable is not the installed binary")
        if _mode_arg(snapshot.argv) != "shipyard" or "daemon" not in snapshot.argv or "run" not in snapshot.argv:
            raise GuardianError(f"production argv is not Shipyard daemon run: {snapshot.argv!r}")
        self.snapshot = snapshot
        self.installed_hash = _sha256(self.installed)
        self.candidate_hash = _sha256(self.candidate)
        self.configured_repos = _configured_repos(
            snapshot,
            self.installed,
            diagnostic_root=self.root,
            state_dir=self.production_state_dir,
        )
        if self.configured_repos != _repo_args(snapshot.argv):
            raise GuardianError("daemon status configured_repos disagrees with exact argv")
        self.worker_ids = _active_runs(
            snapshot,
            self.installed,
            state_dir=self.production_state_dir,
            diagnostic_root=self.root,
        )
        if self.worker_ids:
            raise GuardianError(
                f"refusing canary transition with active workers: {self.worker_ids!r}"
            )
        self.lock_path = self.production_pid_file.parent.parent / ".sandbox-writer-domain.lock"
        old_holders = _lock_holders(
            self.lock_path,
            retry_after_timeout=self.verify_unchanged_production,
            diagnostic_root=self.root,
        )
        old_contended = _exclusive_lock_is_contended(self.lock_path)
        self.transition_path = _select_transition_after_bounded_observation(
            self.lock_path,
            snapshot.pid,
            old_holders,
            old_contended,
            verify_production=self.verify_unchanged_production,
            installed_version=lambda deadline: _running_daemon_version(
                snapshot,
                self.installed,
                self.production_state_dir,
                deadline=deadline,
            ),
            diagnostic_root=self.root,
        )
        if None in (
            self.lease_device,
            self.lease_inode,
            self.lease_ctime_ns,
            self.lease_generation,
        ):
            raise GuardianError("lease generation identity is unavailable at transition")
        try:
            advanced_lease = _advance_lease_generation(
                self.lease_dir,
                (self.lease_device, self.lease_inode, self.lease_ctime_ns),
                self.lease_generation,
            )
        except Exception:
            current_lease, current_generation = _validate_lease_generation(
                self.lease_dir
            )
            if (
                (current_lease.st_dev, current_lease.st_ino)
                == (self.lease_device, self.lease_inode)
                and current_generation == self.lease_generation
            ):
                self.lease_ctime_ns = current_lease.st_ctime_ns
            raise
        self.lease_ctime_ns = advanced_lease.st_ctime_ns
        self.old_lifetime_lock_owned = self.transition_path == LEGACY_TRANSITION
        if self.transition_path == CORRECTED_TRANSITION:
            return

        # Arm restoration before invoking the CLI: it can send stop IPC and
        # then time out before returning, while production exits asynchronously.
        self.production_stop_requested = True
        stop_result = _run(
            [
                str(self.installed),
                "--mode",
                "shipyard",
                "--state-dir",
                str(self.production_state_dir),
                "daemon",
                "stop",
            ],
            cwd="/",
            env=snapshot.environment,
            check=False,
        )
        deadline = time.monotonic() + 15.0
        while _pid_alive(snapshot.pid) and time.monotonic() < deadline:
            time.sleep(0.1)
        if _pid_alive(snapshot.pid):
            detail = (stop_result.stderr or stop_result.stdout).strip()
            raise GuardianError(
                f"production daemon pid {snapshot.pid} did not stop: {detail}"
            )
        # Set this immediately after observing death.  Every later fallible
        # assertion is then covered by the outer restoration path.
        self.production_quiesced = True
        holders = _lock_holders(self.lock_path, diagnostic_root=self.root)
        if holders or _exclusive_lock_is_contended(self.lock_path):
            raise GuardianError(f"old Shipyard processes still own writer domain: {holders!r}")

    def start_candidate(self) -> None:
        safe_env = {
            "HOME": str(Path.home()),
            "USERPROFILE": str(Path.home()),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "RUST_BACKTRACE": "1",
            "SHIPYARD_ENABLE_TUNNEL": "0",
        }
        log = (self.root / "candidate.log").open("ab", buffering=0)
        argv = [
            str(self.candidate),
            "--mode",
            "isolated",
            "--global-dir",
            str(self.global_dir),
            "--state-dir",
            str(self.state_dir),
            "daemon",
            "run",
        ]
        self.candidate_process = subprocess.Popen(
            argv,
            cwd=self.root,
            env=safe_env,
            stdin=subprocess.DEVNULL,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        self.candidate_stopped = False
        deadline = time.monotonic() + 10.0
        while time.monotonic() < deadline:
            if self.candidate_process.poll() is not None:
                raise GuardianError("candidate daemon exited before becoming ready")
            result = _run(
                [
                    str(self.candidate),
                    "--mode",
                    "isolated",
                    "--global-dir",
                    str(self.global_dir),
                    "--state-dir",
                    str(self.state_dir),
                    "--json",
                    "daemon",
                    "status",
                ],
                cwd=self.root,
                env=safe_env,
                check=False,
            )
            if result.returncode == 0:
                status = json.loads(result.stdout)
                if (
                    status.get("running") is True
                    and status.get("tunnel", {}).get("backend") == "inactive"
                    and status.get("configured_repos", []) == []
                    and status.get("registered_repos", []) == []
                ):
                    _atomic_json(
                        self.ready_receipt,
                        {
                            "schema_version": 1,
                            "phase": "ready",
                            "candidate_pid": self.candidate_process.pid,
                            "candidate_sha256": self.candidate_hash,
                            "installed_sha256": self.installed_hash,
                            "production_pid": self.snapshot.pid if self.snapshot else None,
                            "production_start_time": (
                                self.snapshot.start_time if self.snapshot else None
                            ),
                            "transition_path": self.transition_path,
                            "production_executable": (
                                self.snapshot.executable if self.snapshot else None
                            ),
                            "production_argv_sha256": (
                                self.snapshot.argv_sha256 if self.snapshot else None
                            ),
                            "production_environment_sha256": (
                                self.snapshot.environment_sha256 if self.snapshot else None
                            ),
                            "production_cwd": self.snapshot.cwd if self.snapshot else None,
                            "mutation_guard_path": str(self.mutation_guard_path),
                            "configured_repos": self.configured_repos,
                            "active_runs": self.worker_ids,
                        },
                    )
                    return
            time.sleep(0.1)
        raise GuardianError("candidate daemon did not become ready")

    def wait_for_owner(self) -> None:
        while not self.done_file.exists():
            if self.stop_requested:
                raise OwnerEnded("guardian received termination signal")
            if _process_start(self.args.owner_pid) != self.owner_start:
                raise OwnerEnded(OWNER_ENDED_BEFORE_COMPLETION)
            if (
                self.transition_path == CORRECTED_TRANSITION
                and self.audit_ready_file.exists()
                and not self.mutation_fence_proved
            ):
                self.prove_corrected_mutation_fence()
            time.sleep(0.25)

    def assert_process_identity(
        self,
        actual: ProcessSnapshot,
        *,
        require_same_pid: bool,
        require_same_stdio: bool = False,
    ) -> None:
        expected = self.snapshot
        if expected is None:
            raise GuardianError("production snapshot is unavailable")
        if require_same_pid and (
            actual.pid != expected.pid or actual.start_time != expected.start_time
        ):
            raise GuardianError("production daemon pid/start identity changed")
        if (
            actual.executable != expected.executable
            or actual.argv != expected.argv
            or actual.environment_sha256 != expected.environment_sha256
            or actual.cwd != expected.cwd
        ):
            raise GuardianError("production daemon process identity differs from snapshot")
        if require_same_stdio and (
            actual.stdin_path != expected.stdin_path
            or actual.stdout_path != expected.stdout_path
            or actual.stderr_path != expected.stderr_path
        ):
            raise GuardianError("restored daemon stdio identity differs from snapshot")

    def verify_unchanged_production(
        self, deadline: Optional[float] = None
    ) -> ProcessSnapshot:
        """Fail closed if production changes during a lock observation window."""
        snapshot = self.snapshot
        if snapshot is None:
            raise GuardianError("production snapshot is unavailable")
        _bounded_timeout(deadline)
        if _sha256(self.installed) != self.installed_hash:
            raise GuardianError("installed production binary changed during idle wait")
        _bounded_timeout(deadline)
        current_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
        if current_pid != snapshot.pid:
            raise GuardianError("production daemon pid changed during idle wait")
        current = snapshot_process(current_pid, deadline=deadline)
        self.assert_process_identity(current, require_same_pid=True)
        if (
            _configured_repos(
                current,
                self.installed,
                deadline=deadline,
                diagnostic_root=self.root,
                state_dir=self.production_state_dir,
            )
            != self.configured_repos
        ):
            raise GuardianError(
                "production configured repository authority changed during idle wait"
            )
        if (
            _active_runs(
                current,
                self.installed,
                state_dir=self.production_state_dir,
                deadline=deadline,
                diagnostic_root=self.root,
            )
            != self.worker_ids
        ):
            raise GuardianError("production active workers changed during idle wait")
        _bounded_timeout(deadline)
        return current

    def prove_corrected_mutation_fence(self) -> None:
        snapshot = self.snapshot
        if snapshot is None:
            raise GuardianError("production snapshot is unavailable")
        if self.mutation_guard_path.exists() or self.mutation_probe_output.exists():
            raise GuardianError(
                "mutation proof paths already exist: "
                f"guard={self.mutation_guard_path}, output={self.mutation_probe_output}"
            )
        holders = _lock_holders(
            self.lock_path,
            retry_after_timeout=self.verify_unchanged_production,
            diagnostic_root=self.root,
        )
        audit_holders = tuple(pid for pid in holders if pid != snapshot.pid)
        if not audit_holders or not _exclusive_lock_is_contended(self.lock_path):
            raise GuardianError(
                f"exclusive audit was not proven against corrected daemon: {holders!r}"
            )
        probe_environment = dict(snapshot.environment)
        # The daemon itself carries this marker so its detached log writes join
        # the writer domain.  The foreground proof CLI must report the overlap
        # on its captured pipe rather than trying to acquire the same exclusive
        # audit merely to print the diagnostic.
        probe_environment.pop(PROTECTED_STDIO_PATH_ENV, None)
        result = _run_status_probe(
            [
                str(self.installed),
                "--mode",
                "shipyard",
                "writer-domain-exec",
                "--path",
                str(self.mutation_guard_path),
                "--",
                "/usr/bin/touch",
                str(self.mutation_probe_output),
            ],
            cwd=snapshot.cwd,
            env=probe_environment,
            timeout=40.0,
            diagnostic_root=self.root,
            production_pid=snapshot.pid,
        )
        combined = f"{result.stdout}\n{result.stderr}"
        if (
            result.returncode != WRITER_DOMAIN_OVERLAP_EXIT_CODE
            or WRITER_DOMAIN_OVERLAP_CLASSIFICATION not in combined
            or self.mutation_guard_path.exists()
            or self.mutation_probe_output.exists()
        ):
            raise GuardianError(
                "corrected production mutation was not fenced by the exclusive audit: "
                f"returncode={result.returncode}, output={combined.strip()!r}"
            )
        current_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
        if current_pid != snapshot.pid:
            raise GuardianError("corrected production pid file changed during audit")
        current = snapshot_process(current_pid)
        self.assert_process_identity(current, require_same_pid=True)
        if (
            _configured_repos(
                current,
                self.installed,
                diagnostic_root=self.root,
                state_dir=self.production_state_dir,
            )
            != self.configured_repos
        ):
            raise GuardianError("corrected production configured repositories changed")
        if (
            _active_runs(
                current,
                self.installed,
                state_dir=self.production_state_dir,
                diagnostic_root=self.root,
            )
            != self.worker_ids
        ):
            raise GuardianError("corrected production active workers changed")
        _atomic_json(
            self.mutation_receipt,
            {
                "schema_version": 1,
                "transition_path": self.transition_path,
                "selected_protected_path": str(self.mutation_guard_path),
                "probe_output": str(self.mutation_probe_output),
                "production_pid": snapshot.pid,
                "production_start_time": snapshot.start_time,
                "installed_sha256": self.installed_hash,
                "argv_sha256": snapshot.argv_sha256,
                "environment_sha256": snapshot.environment_sha256,
                "cwd": snapshot.cwd,
                "returncode": result.returncode,
                "overlap_classification": WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
                "mutation_absent": True,
                "selected_path_absent": True,
                "production_identity_preserved": True,
            },
        )
        self.mutation_fence_proved = True

    def stop_candidate(self) -> None:
        if self.candidate_process is None:
            return
        safe_env = {
            "HOME": str(Path.home()),
            "USERPROFILE": str(Path.home()),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_ENABLE_TUNNEL": "0",
        }
        _run(
            [
                str(self.candidate),
                "--mode",
                "isolated",
                "--global-dir",
                str(self.global_dir),
                "--state-dir",
                str(self.state_dir),
                "daemon",
                "stop",
            ],
            cwd=self.root,
            env=safe_env,
            check=False,
        )
        try:
            self.candidate_process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.candidate_process.terminate()
            try:
                self.candidate_process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.candidate_process.kill()
                self.candidate_process.wait(timeout=5)
        self.candidate_stopped = not _pid_alive(self.candidate_process.pid)
        if not self.candidate_stopped:
            raise GuardianError("candidate daemon survived cleanup")

    def finalize_production(self) -> None:
        if self.transition_path == CORRECTED_TRANSITION:
            self.verify_preserved_production()
        else:
            self.restore_legacy_production()

    def verify_preserved_production(
        self, *, require_mutation_fence: bool = True
    ) -> None:
        snapshot = self.snapshot
        if snapshot is None:
            raise GuardianError("production snapshot is unavailable")
        if require_mutation_fence and not self.mutation_fence_proved:
            raise GuardianError("corrected transition lacks exclusive-audit mutation proof")
        if _sha256(self.installed) != self.installed_hash:
            raise GuardianError("installed production binary changed during canary")
        current_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
        if current_pid != snapshot.pid:
            raise GuardianError("corrected production pid changed during canary")
        current = snapshot_process(current_pid)
        self.assert_process_identity(current, require_same_pid=True)
        if (
            _configured_repos(
                current,
                self.installed,
                diagnostic_root=self.root,
                state_dir=self.production_state_dir,
            )
            != self.configured_repos
        ):
            raise GuardianError("preserved configured repository authority differs")
        if (
            _active_runs(
                current,
                self.installed,
                state_dir=self.production_state_dir,
                diagnostic_root=self.root,
            )
            != self.worker_ids
        ):
            raise GuardianError(PRESERVED_WORKER_OWNERSHIP_FAILURE)
        _wait_for_idle_writer_domain(
            self.lock_path,
            snapshot.pid,
            verify_production=self.verify_unchanged_production,
            diagnostic_root=self.root,
        )
        current = self.verify_unchanged_production()
        self.restored_pid = current.pid
        self.final_production_start_time = current.start_time
        self.production_preserved = True
        self.production_identity_verified = True

    def restore_legacy_production(self) -> None:
        snapshot = self.snapshot
        if snapshot is None:
            return
        if _sha256(self.installed) != self.installed_hash:
            raise GuardianError("installed production binary changed during canary")
        try:
            existing_pid = int(
                self.production_pid_file.read_text(encoding="utf-8").strip()
            )
        except (FileNotFoundError, ValueError):
            existing_pid = None
        if existing_pid is not None and _pid_alive(existing_pid):
            same_stop_requested_generation = False
            if self.production_stop_requested and existing_pid == snapshot.pid:
                current_generation = snapshot_process(existing_pid)
                same_stop_requested_generation = (
                    current_generation.start_time == snapshot.start_time
                )
            if same_stop_requested_generation:
                stop_deadline = time.monotonic() + 15.0
                while _pid_alive(existing_pid) and time.monotonic() < stop_deadline:
                    time.sleep(0.1)
                if _pid_alive(existing_pid):
                    raise GuardianError(
                        f"stop-requested production pid {existing_pid} is still exiting"
                    )
            else:
                retained = self.restoration_process
                if retained is not None and retained.pid != existing_pid:
                    self.stop_conflicting_restoration_process(retained)
                self.verify_and_adopt_restored_production(existing_pid)
                return
        process = self.restoration_process
        if process is not None and process.poll() is not None:
            self.restoration_process = None
            process = None
        if process is None:
            stdin = open(snapshot.stdin_path, "rb", buffering=0)
            stdout = open(snapshot.stdout_path, "ab", buffering=0)
            stderr = stdout if snapshot.stderr_path == snapshot.stdout_path else open(
                snapshot.stderr_path, "ab", buffering=0
            )
            try:
                process = subprocess.Popen(
                    list(snapshot.argv),
                    executable=snapshot.executable,
                    cwd=snapshot.cwd,
                    env=snapshot.environment,
                    stdin=stdin,
                    stdout=stdout,
                    stderr=stderr,
                    start_new_session=True,
                )
            finally:
                stdin.close()
                stdout.close()
                if stderr is not stdout:
                    stderr.close()
            self.restoration_process = process
        deadline = time.monotonic() + 15.0
        restored_pid = None
        while time.monotonic() < deadline:
            returncode = process.poll()
            if returncode is not None:
                self.restoration_process = None
                raise GuardianError(f"restored daemon exited with {returncode}")
            try:
                restored_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
            except (FileNotFoundError, ValueError):
                time.sleep(0.1)
                continue
            if restored_pid == process.pid:
                self.verify_and_adopt_restored_production(process.pid)
                return
            time.sleep(0.1)
        raise GuardianError("restored daemon did not own the production pid file")

    def stop_conflicting_restoration_process(
        self, process: subprocess.Popen
    ) -> None:
        """Authoritatively reap Shipyard's retained child before adopting another."""
        if process.poll() is None:
            try:
                process.terminate()
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                try:
                    process.kill()
                except ProcessLookupError:
                    pass
                process.wait(timeout=5)
        if process.poll() is None or _pid_alive(process.pid):
            raise GuardianError(
                f"conflicting retained restore child {process.pid} survived cleanup"
            )
        self.restoration_process = None

    def verify_and_adopt_restored_production(self, restored_pid: int) -> None:
        def verify_restored_identity(deadline: float) -> ProcessSnapshot:
            _bounded_timeout(deadline)
            current_pid = int(
                self.production_pid_file.read_text(encoding="utf-8").strip()
            )
            if current_pid != restored_pid:
                raise GuardianError("restored daemon did not own the production pid file")
            current = snapshot_process(restored_pid, deadline=deadline)
            self.assert_process_identity(
                current, require_same_pid=False, require_same_stdio=True
            )
            _bounded_timeout(deadline)
            return current

        deadline = time.monotonic() + 15.0
        restored: Optional[ProcessSnapshot] = None
        while time.monotonic() < deadline:
            restored = verify_restored_identity(deadline)
            try:
                status = _peer_verified_daemon_status(
                    self.production_state_dir,
                    restored_pid,
                    deadline=deadline,
                )
            except (GuardianError, OSError, socket.timeout):
                time.sleep(0.1)
                continue
            if status.get("running") is True:
                version = status.get("shipyard_version")
                if version != LEGACY_LIFETIME_LOCK_VERSION:
                    raise GuardianError(
                        "restored daemon did not report the exact legacy "
                        f"version: {version!r}"
                    )
                break
            time.sleep(0.1)
        else:
            raise GuardianError(
                f"restored daemon pid {restored_pid} never reported running"
            )
        assert restored is not None
        current = verify_restored_identity(deadline)
        if current.start_time != restored.start_time:
            raise GuardianError("restored daemon identity changed during status verification")
        restored = current
        if (
            _configured_repos(
                restored,
                self.installed,
                diagnostic_root=self.root,
                state_dir=self.production_state_dir,
            )
            != self.configured_repos
        ):
            raise GuardianError("restored configured repository authority differs")
        if (
            _active_runs(
                restored,
                self.installed,
                state_dir=self.production_state_dir,
                diagnostic_root=self.root,
            )
            != self.worker_ids
        ):
            raise GuardianError("restored active worker ownership differs")
        lock_deadline = time.monotonic() + 15.0
        stable_lifetime_lock = 0
        while time.monotonic() < lock_deadline:
            current = verify_restored_identity(lock_deadline)
            if current.start_time != restored.start_time:
                raise GuardianError("restored daemon identity changed before lock proof")
            restored_holders = _lock_holders(
                self.lock_path,
                deadline=lock_deadline,
                retry_after_timeout=lambda retry_deadline: verify_restored_identity(
                    retry_deadline
                ),
                diagnostic_root=self.root,
            )
            foreign_holders = tuple(
                pid for pid in restored_holders if pid != restored_pid
            )
            if foreign_holders:
                raise GuardianError(
                    "foreign process entered restored legacy writer domain: "
                    f"{foreign_holders!r}"
                )
            restored_contended = _exclusive_lock_is_contended(self.lock_path)
            current = verify_restored_identity(lock_deadline)
            if current.start_time != restored.start_time:
                raise GuardianError("restored daemon identity changed during lock proof")
            _bounded_timeout(lock_deadline)
            if restored_holders == (restored_pid,) and restored_contended:
                stable_lifetime_lock += 1
                if stable_lifetime_lock >= 3:
                    break
            else:
                stable_lifetime_lock = 0
            time.sleep(0.1)
        else:
            raise GuardianError(
                f"restored legacy daemon pid {restored_pid} did not prove stable "
                "lifetime-lock ownership"
            )
        if (
            _configured_repos(
                current,
                self.installed,
                deadline=lock_deadline,
                state_dir=self.production_state_dir,
            )
            != self.configured_repos
        ):
            raise GuardianError(
                "restored configured repository authority changed during lock proof"
            )
        if (
            _active_runs(
                current,
                self.installed,
                state_dir=self.production_state_dir,
                deadline=lock_deadline,
            )
            != self.worker_ids
        ):
            raise GuardianError("restored active workers changed during lock proof")
        current = verify_restored_identity(lock_deadline)
        if current.start_time != restored.start_time:
            raise GuardianError("restored daemon identity changed after lock proof")
        _bounded_timeout(lock_deadline)
        self.restored_pid = restored_pid
        self.final_production_start_time = restored.start_time
        self.production_restored = True
        self.production_identity_verified = True

    def release(self) -> None:
        if self.restoration_outstanding():
            raise GuardianError(
                "refusing to release host lease before production identity is verified"
            )
        if self.lease_owned:
            if None in (
                self.lease_device,
                self.lease_inode,
                self.lease_ctime_ns,
                self.lease_generation,
            ):
                raise GuardianError("host lease generation identity is unavailable")
            _remove_generation_bound_lease(
                self.lease_dir,
                (
                    self.lease_device,
                    self.lease_inode,
                    self.lease_ctime_ns,
                ),
                self.lease_generation,
            )
            self.lease_owned = False

    def restoration_outstanding(self) -> bool:
        return (
            hasattr(self, "transition_path")
            and not self.production_identity_verified
            and (
                self.production_stop_requested
                or self.production_quiesced
                or self.transition_path == CORRECTED_TRANSITION
            )
        )

    def run(self) -> int:
        signal.signal(signal.SIGTERM, self.request_stop)
        signal.signal(signal.SIGINT, self.request_stop)
        reason = "completed"
        try:
            run_lifecycle(
                self.acquire,
                self.preflight_and_transition,
                self.start_candidate,
                self.wait_for_owner,
                self.stop_candidate,
                self.finalize_production,
                self.release,
            )
        except ReconciledAfterOwnerEnded:
            reason = RETAINED_RECONCILED_REASON
            self.production_preserved = True
            self.production_identity_verified = True
            assert self.snapshot is not None
            self.restored_pid = self.snapshot.pid
            self.final_production_start_time = self.snapshot.start_time
        except OwnerEnded as error:
            reason = "owner-ended"
            self.failure = str(error)
        except Exception as error:  # fail closed, but always publish restoration truth
            reason = "failed"
            self.failure = f"{type(error).__name__}: {error}"
        finally:
            # A partial lifecycle failure must not short-circuit a later cleanup
            # obligation.  Retry only obligations whose authoritative state says
            # they remain outstanding, and collect every failure for the receipt.
            cleanup_failures: list[str] = []
            if self.candidate_process is not None and not self.candidate_stopped:
                try:
                    self.stop_candidate()
                except Exception as error:
                    cleanup_failures.append(
                        f"stop candidate: {type(error).__name__}: {error}"
                    )
            restoration_outstanding = self.restoration_outstanding()
            if restoration_outstanding:
                try:
                    if (
                        self.transition_path == CORRECTED_TRANSITION
                        and not self.mutation_fence_proved
                    ):
                        self.verify_preserved_production(
                            require_mutation_fence=False
                        )
                    else:
                        self.finalize_production()
                except Exception as error:
                    cleanup_failures.append(
                        f"restore production: {type(error).__name__}: {error}"
                    )
            restoration_outstanding = self.restoration_outstanding()
            if self.lease_owned and not restoration_outstanding:
                try:
                    self.release()
                except Exception as error:
                    cleanup_failures.append(
                        f"release lease: {type(error).__name__}: {error}"
                    )
            if cleanup_failures:
                prefix = f"{self.failure}; " if self.failure else ""
                self.failure = prefix + "; ".join(cleanup_failures)
            final_payload = {
                    "schema_version": 1,
                    "reason": reason,
                    "failure": self.failure,
                    "candidate_stopped": self.candidate_stopped,
                    "production_quiesced": self.production_quiesced,
                    "production_restored": self.production_restored,
                    "production_preserved": self.production_preserved,
                    "production_identity_verified": self.production_identity_verified,
                    "lease_retained": self.lease_owned,
                    "lease_device": self.lease_device,
                    "lease_inode": self.lease_inode,
                    "lease_ctime_ns": self.lease_ctime_ns,
                    "lease_generation": self.lease_generation,
                    "transition_path": getattr(self, "transition_path", None),
                    "old_production_pid": self.snapshot.pid if self.snapshot else None,
                    "old_production_start_time": (
                        self.snapshot.start_time if self.snapshot else None
                    ),
                    "restored_production_pid": self.restored_pid,
                    "final_production_pid": self.restored_pid,
                    "final_production_start_time": self.final_production_start_time,
                    "installed_sha256": getattr(self, "installed_hash", None),
                    "candidate_sha256": getattr(self, "candidate_hash", None),
                    "argv_sha256": self.snapshot.argv_sha256 if self.snapshot else None,
                    "environment_sha256": (
                        self.snapshot.environment_sha256 if self.snapshot else None
                    ),
                    "cwd": self.snapshot.cwd if self.snapshot else None,
                    "mode": _mode_arg(self.snapshot.argv) if self.snapshot else None,
                    "configured_repos": getattr(self, "configured_repos", ()),
                    "active_runs": getattr(self, "worker_ids", ()),
                    "old_lifetime_lock_owned": getattr(
                        self, "old_lifetime_lock_owned", False
                    ),
                    "mutation_fence_proved": self.mutation_fence_proved,
                    "mutation_guard_path": str(self.mutation_guard_path),
                    "mutation_probe_output": str(self.mutation_probe_output),
                    "lease_removed": not self.lease_dir.exists(),
                    "reconciled_prior_canary_root": self.reconciled_prior_canary_root,
                }
            if reason == RETAINED_RECONCILED_REASON:
                _durable_atomic_json(self.final_receipt, final_payload)
            else:
                _atomic_json(self.final_receipt, final_payload)
        production_ready = self.production_restored or self.production_preserved
        return 0 if production_ready and self.candidate_stopped and not self.failure else 1


def parse_args(argv: Optional[list[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--owner-pid", type=int, required=True)
    parser.add_argument("--installed", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--global-dir", required=True)
    parser.add_argument("--state-dir", required=True)
    parser.add_argument("--canary-root", required=True)
    parser.add_argument("--done-file", required=True)
    parser.add_argument("--ready-receipt", required=True)
    parser.add_argument("--final-receipt", required=True)
    parser.add_argument("--lease-dir", required=True)
    parser.add_argument("--production-pid-file", required=True)
    return parser.parse_args(argv)


def main(argv: Optional[list[str]] = None) -> NoReturn:
    raise SystemExit(Guardian(parse_args(argv)).run())


if __name__ == "__main__":
    main()
