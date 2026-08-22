#!/usr/bin/env python3
"""Host-owned transaction guardian for the M3 Sandbox canary.

The guardian, rather than the Actions shell, creates the machine-wide canary
lease.  Once it owns that lease it is responsible for restoring the exact
production daemon process even when the launching shell disappears.
"""

from __future__ import annotations

import argparse
import ctypes
import ctypes.util
import fcntl
import hashlib
import json
import os
import signal
import struct
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, NoReturn, Optional, Union


class GuardianError(RuntimeError):
    """Fail-closed canary transaction error."""


class OwnerEnded(RuntimeError):
    """The Actions owner exited or was cancelled before writing done."""


LEGACY_TRANSITION = "legacy-lifetime-lock-quiesce-restore"
CORRECTED_TRANSITION = "corrected-idle-preserve-fence"
WRITER_DOMAIN_OVERLAP_EXIT_CODE = 75
WRITER_DOMAIN_OVERLAP_CLASSIFICATION = "sandbox_writer_domain_overlap"
PROTECTED_STDIO_PATH_ENV = "SHIPYARD_PROTECTED_STDIO_PATH"


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


def _lsof_field(pid: int, descriptor: str) -> str:
    result = _run(
        ["/usr/sbin/lsof", "-a", "-p", str(pid), "-d", descriptor, "-Fn"],
        cwd="/",
        env={"PATH": "/usr/bin:/bin:/usr/sbin:/sbin"},
    )
    values = [line[1:] for line in result.stdout.splitlines() if line.startswith("n")]
    if len(values) != 1:
        raise GuardianError(f"could not resolve fd {descriptor} for pid {pid}: {values!r}")
    return values[0]


def snapshot_process(pid: int) -> ProcessSnapshot:
    executable, argv, environment = _darwin_argv_environment(pid)
    start_time = _process_start(pid)
    if start_time is None:
        raise GuardianError(f"process {pid} disappeared while being snapshotted")
    return ProcessSnapshot(
        pid=pid,
        executable=executable,
        argv=argv,
        environment=environment,
        cwd=_lsof_field(pid, "cwd"),
        stdin_path=_lsof_field(pid, "0"),
        stdout_path=_lsof_field(pid, "1"),
        stderr_path=_lsof_field(pid, "2"),
        start_time=start_time,
    )


def _pid_alive(pid: int) -> bool:
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False


def _process_start(pid: int) -> Optional[str]:
    result = subprocess.run(
        ["/bin/ps", "-p", str(pid), "-o", "lstart="],
        capture_output=True,
        text=True,
        check=False,
    )
    value = result.stdout.strip()
    return value or None


def _lock_holders(path: Path) -> tuple[int, ...]:
    result = subprocess.run(
        ["/usr/sbin/lsof", "-t", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )
    holders: list[int] = []
    for value in result.stdout.split():
        try:
            holders.append(int(value))
        except ValueError as error:
            raise GuardianError(f"invalid lsof pid for {path}: {value!r}") from error
    return tuple(sorted(set(holders)))


def _exclusive_lock_is_contended(path: Path) -> bool:
    with path.open("a+b") as handle:
        try:
            fcntl.flock(handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
        return False


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


def _json_command(snapshot: ProcessSnapshot, installed: Path, *args: str) -> dict[str, object]:
    result = _run(
        [str(installed), "--json", *args],
        cwd=snapshot.cwd,
        env=snapshot.environment,
    )
    value = json.loads(result.stdout)
    if not isinstance(value, dict):
        raise GuardianError(f"expected JSON object from {args!r}")
    return value


def _active_runs(snapshot: ProcessSnapshot, installed: Path) -> tuple[str, ...]:
    status = _json_command(snapshot, installed, "status")
    runs = status.get("active_runs")
    if not isinstance(runs, list):
        raise GuardianError("worker status active_runs is not an array")
    return tuple(sorted(str(run["id"]) for run in runs if isinstance(run, dict) and "id" in run))


def _configured_repos(snapshot: ProcessSnapshot, installed: Path) -> tuple[str, ...]:
    status = _json_command(snapshot, installed, "daemon", "status")
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
    for enabled, name, action in (
        (candidate_started, "stop candidate", stop_candidate),
        (quiesced, "restore production", restore),
        (acquired, "release lease", release),
    ):
        if not enabled:
            continue
        try:
            action()
        except Exception as error:
            cleanup_failures.append(f"{name}: {type(error).__name__}: {error}")

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
        self.production_pid_file = Path(args.production_pid_file)
        self.audit_ready_file = self.root / "exclusive-audit-ready"
        self.mutation_receipt = self.root / "mutation-fence.json"
        self.mutation_guard_path = (
            self.production_pid_file.parent.parent
            / f".sandbox-canary-guard-{self.root.name}"
        )
        self.mutation_probe_output = self.root / "unexpected-mutation-ran"
        self.snapshot: Optional[ProcessSnapshot] = None
        self.candidate_process: Optional[subprocess.Popen] = None
        self.restored_pid: Optional[int] = None
        self.final_production_start_time: Optional[str] = None
        self.owner_start = _process_start(args.owner_pid)
        self.stop_requested = False
        self.lease_owned = False
        self.production_quiesced = False
        self.production_restored = False
        self.production_preserved = False
        self.production_identity_verified = False
        self.mutation_fence_proved = False
        self.candidate_stopped = True
        self.failure: Optional[str] = None

    def request_stop(self, _signum: int, _frame: object) -> None:
        self.stop_requested = True

    def acquire(self) -> None:
        # Arm cleanup before the atomic mkdir.  Python signal handlers run only
        # between bytecodes, so after mkdir succeeds there is no bytecode where
        # the host lease exists but the guardian does not own its cleanup.
        self.lease_owned = True
        try:
            os.mkdir(self.lease_dir, 0o700)
        except Exception:
            self.lease_owned = False
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
        self.configured_repos = _configured_repos(snapshot, self.installed)
        if self.configured_repos != _repo_args(snapshot.argv):
            raise GuardianError("daemon status configured_repos disagrees with exact argv")
        self.worker_ids = _active_runs(snapshot, self.installed)
        if self.worker_ids:
            raise GuardianError(
                f"refusing canary transition with active workers: {self.worker_ids!r}"
            )
        self.lock_path = self.production_pid_file.parent.parent / ".sandbox-writer-domain.lock"
        old_holders = _lock_holders(self.lock_path)
        self.transition_path = _select_transition(
            snapshot.pid,
            old_holders,
            _exclusive_lock_is_contended(self.lock_path),
        )
        self.old_lifetime_lock_owned = self.transition_path == LEGACY_TRANSITION
        if self.transition_path == CORRECTED_TRANSITION:
            return

        stop_result = _run(
            [str(self.installed), "--mode", "shipyard", "daemon", "stop"],
            cwd=snapshot.cwd,
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
        holders = _lock_holders(self.lock_path)
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
                raise OwnerEnded("Actions owner ended before canary completion")
            if (
                self.transition_path == CORRECTED_TRANSITION
                and self.audit_ready_file.exists()
                and not self.mutation_fence_proved
            ):
                self.prove_corrected_mutation_fence()
            time.sleep(0.25)

    def assert_process_identity(
        self, actual: ProcessSnapshot, *, require_same_pid: bool
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

    def prove_corrected_mutation_fence(self) -> None:
        snapshot = self.snapshot
        if snapshot is None:
            raise GuardianError("production snapshot is unavailable")
        if self.mutation_guard_path.exists() or self.mutation_probe_output.exists():
            raise GuardianError(
                "mutation proof paths already exist: "
                f"guard={self.mutation_guard_path}, output={self.mutation_probe_output}"
            )
        holders = _lock_holders(self.lock_path)
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
        result = _run(
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
            check=False,
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
        if _configured_repos(current, self.installed) != self.configured_repos:
            raise GuardianError("corrected production configured repositories changed")
        if _active_runs(current, self.installed) != self.worker_ids:
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

    def verify_preserved_production(self) -> None:
        snapshot = self.snapshot
        if snapshot is None:
            raise GuardianError("production snapshot is unavailable")
        if not self.mutation_fence_proved:
            raise GuardianError("corrected transition lacks exclusive-audit mutation proof")
        if _sha256(self.installed) != self.installed_hash:
            raise GuardianError("installed production binary changed during canary")
        current_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
        if current_pid != snapshot.pid:
            raise GuardianError("corrected production pid changed during canary")
        current = snapshot_process(current_pid)
        self.assert_process_identity(current, require_same_pid=True)
        if _configured_repos(current, self.installed) != self.configured_repos:
            raise GuardianError("preserved configured repository authority differs")
        if _active_runs(current, self.installed) != self.worker_ids:
            raise GuardianError("preserved active worker ownership differs")
        holders = _lock_holders(self.lock_path)
        if holders not in ((), (snapshot.pid,)) or _exclusive_lock_is_contended(
            self.lock_path
        ):
            raise GuardianError(
                f"corrected daemon acquired an idle lifetime lock: {holders!r}"
            )
        self.restored_pid = current_pid
        self.final_production_start_time = current.start_time
        self.production_preserved = True
        self.production_identity_verified = True

    def restore_legacy_production(self) -> None:
        snapshot = self.snapshot
        if snapshot is None:
            return
        if _sha256(self.installed) != self.installed_hash:
            raise GuardianError("installed production binary changed during canary")
        stdin = open(snapshot.stdin_path, "rb", buffering=0)
        stdout = open(snapshot.stdout_path, "ab", buffering=0)
        stderr = stdout if snapshot.stderr_path == snapshot.stdout_path else open(
            snapshot.stderr_path, "ab", buffering=0
        )
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
        deadline = time.monotonic() + 15.0
        restored_pid = None
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise GuardianError(f"restored daemon exited with {process.returncode}")
            try:
                restored_pid = int(self.production_pid_file.read_text(encoding="utf-8").strip())
            except (FileNotFoundError, ValueError):
                time.sleep(0.1)
                continue
            if restored_pid == process.pid:
                try:
                    status = _json_command(snapshot, self.installed, "daemon", "status")
                except (GuardianError, json.JSONDecodeError):
                    time.sleep(0.1)
                    continue
                if status.get("running") is True:
                    break
            time.sleep(0.1)
        if restored_pid != process.pid:
            raise GuardianError("restored daemon did not own the production pid file")
        restored = snapshot_process(process.pid)
        self.assert_process_identity(restored, require_same_pid=False)
        if _configured_repos(restored, self.installed) != self.configured_repos:
            raise GuardianError("restored configured repository authority differs")
        if _active_runs(restored, self.installed) != self.worker_ids:
            raise GuardianError("restored active worker ownership differs")
        restored_holders = _lock_holders(self.lock_path)
        if restored_holders != (process.pid,) or not _exclusive_lock_is_contended(
            self.lock_path
        ):
            raise GuardianError(
                f"restored daemon pid {process.pid} is not the sole lifetime-lock owner: {restored_holders!r}"
            )
        self.restored_pid = process.pid
        self.final_production_start_time = restored.start_time
        self.production_restored = True
        self.production_identity_verified = True

    def release(self) -> None:
        if self.lease_owned:
            os.rmdir(self.lease_dir)
            self.lease_owned = False

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
            transition_selected = hasattr(self, "transition_path")
            if (
                transition_selected
                and not self.production_identity_verified
                and (
                    self.production_quiesced
                    or self.transition_path == CORRECTED_TRANSITION
                )
            ):
                try:
                    self.finalize_production()
                except Exception as error:
                    cleanup_failures.append(
                        f"restore production: {type(error).__name__}: {error}"
                    )
            if self.lease_owned:
                try:
                    self.release()
                except Exception as error:
                    cleanup_failures.append(
                        f"release lease: {type(error).__name__}: {error}"
                    )
            if cleanup_failures:
                prefix = f"{self.failure}; " if self.failure else ""
                self.failure = prefix + "; ".join(cleanup_failures)
            _atomic_json(
                self.final_receipt,
                {
                    "schema_version": 1,
                    "reason": reason,
                    "failure": self.failure,
                    "candidate_stopped": self.candidate_stopped,
                    "production_quiesced": self.production_quiesced,
                    "production_restored": self.production_restored,
                    "production_preserved": self.production_preserved,
                    "production_identity_verified": self.production_identity_verified,
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
                },
            )
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
