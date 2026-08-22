"""Sandbox harness for Shipyard binary E2E tests."""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import BinaryIO, Mapping, Sequence

if os.name == "nt":
    import ctypes
    import msvcrt
else:
    import fcntl


BINARY_NAME = "shipyard.exe" if sys.platform == "win32" else "shipyard"
PYTHON_BINARY_NAME = "shipyard-py"
WRITER_DOMAIN_OVERLAP_CLASSIFICATION = "sandbox_writer_domain_overlap"
WRITER_DOMAIN_LOCK_NAME = ".sandbox-writer-domain.lock"
WRITER_DOMAIN_ACQUIRE_TIMEOUT_SECONDS = 5.0

PROTECTED_PATHS: tuple[Path, ...] = (
    Path.home() / "Library" / "Application Support" / "shipyard",
    Path.home() / "Library" / "Application Support" / "shipyard-dev",
    Path.home() / ".config" / "shipyard",
    Path.home() / ".config" / "shipyard-dev",
    Path.home() / ".local" / "state" / "shipyard",
    Path.home() / ".local" / "state" / "shipyard-dev",
    Path.home() / "AppData" / "Local" / "shipyard",
    Path.home() / "AppData" / "Local" / "shipyard-dev",
    Path.home() / ".local" / "bin",
    Path.home() / ".shipyard",
    Path.home() / ".shipyard-dev",
    Path.home() / ".cache" / "shipyard",
    Path.home() / ".cache" / "shipyard-dev",
)

DESTRUCTIVE_TOP_LEVEL: frozenset[str] = frozenset({
    "ship",
    "run",
    "auto-merge",
    "wait",
})

DESTRUCTIVE_DAEMON: frozenset[str] = frozenset({
    "start",
    "run",
    "refresh",
    "stop",
})

DESTRUCTIVE_CLOUD: frozenset[tuple[str, ...]] = frozenset({
    ("add-lane",),
    ("retarget",),
    ("handoff", "run"),
})


@dataclass(frozen=True)
class RunResult:
    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str

    def expect_success(self) -> "RunResult":
        if self.returncode != 0:
            raise AssertionError(
                f"command failed rc={self.returncode}: {' '.join(self.argv)}\n"
                f"stdout: {self.stdout}\nstderr: {self.stderr}"
            )
        return self

    @property
    def combined_output(self) -> str:
        return self.stdout + self.stderr

    def json_stdout(self) -> object:
        return json.loads(self.stdout)


@dataclass(frozen=True)
class PythonShipyardSource:
    repo_root: Path
    python: Path


@dataclass(frozen=True)
class ContaminationReport:
    offenders: tuple[Path, ...]
    sentinel_mtime: float

    @property
    def clean(self) -> bool:
        return not self.offenders

    def format(self) -> str:
        if self.clean:
            return "clean"
        lines = ["sandbox wrote outside its isolated HOME/PATH:"]
        lines.extend(f"  {path}" for path in self.offenders)
        lines.append(f"sentinel_mtime={self.sentinel_mtime}")
        return "\n".join(lines)


def _snapshot_paths(root: Path) -> set[Path]:
    if not root.exists():
        return set()
    try:
        return set(root.rglob("*"))
    except OSError:
        return set()


def production_writer_domain_lock_path(home: Path | None = None) -> Path:
    """Return the Rust CLI's host-global production writer-domain lock path."""
    root = home or Path.home()
    if sys.platform == "darwin":
        state_dir = root / "Library" / "Application Support" / "shipyard"
    elif sys.platform == "win32":
        state_dir = root / "AppData" / "Local" / "shipyard"
    else:
        state_dir = root / ".local" / "state" / "shipyard"
    return state_dir / WRITER_DOMAIN_LOCK_NAME


class WriterDomainOverlap(RuntimeError):
    """A proven production-writer/sandbox-audit lease overlap."""


if os.name == "nt":
    class _Overlapped(ctypes.Structure):
        _fields_ = [
            ("Internal", ctypes.c_void_p),
            ("InternalHigh", ctypes.c_void_p),
            ("Offset", ctypes.c_uint32),
            ("OffsetHigh", ctypes.c_uint32),
            ("hEvent", ctypes.c_void_p),
        ]


def _try_writer_domain_lock(file: BinaryIO, *, exclusive: bool) -> bool:
    if os.name != "nt":
        operation = fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH
        try:
            fcntl.flock(file.fileno(), operation | fcntl.LOCK_NB)
            return True
        except BlockingIOError:
            return False

    handle = msvcrt.get_osfhandle(file.fileno())
    overlapped = _Overlapped()
    flags = 0x00000001  # LOCKFILE_FAIL_IMMEDIATELY
    if exclusive:
        flags |= 0x00000002  # LOCKFILE_EXCLUSIVE_LOCK
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    locked = kernel32.LockFileEx(
        ctypes.c_void_p(handle),
        ctypes.c_uint32(flags),
        ctypes.c_uint32(0),
        ctypes.c_uint32(1),
        ctypes.c_uint32(0),
        ctypes.byref(overlapped),
    )
    if locked:
        return True
    error = ctypes.get_last_error()
    if error in (32, 33):
        return False
    raise OSError(error, "LockFileEx failed")


def _unlock_writer_domain(file: BinaryIO) -> None:
    if os.name != "nt":
        fcntl.flock(file.fileno(), fcntl.LOCK_UN)
        return
    handle = msvcrt.get_osfhandle(file.fileno())
    overlapped = _Overlapped()
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    unlocked = kernel32.UnlockFileEx(
        ctypes.c_void_p(handle),
        ctypes.c_uint32(0),
        ctypes.c_uint32(1),
        ctypes.c_uint32(0),
        ctypes.byref(overlapped),
    )
    if not unlocked:
        error = ctypes.get_last_error()
        raise OSError(error, "UnlockFileEx failed")


class WriterDomainLease:
    """A bounded OS-backed shared or exclusive host writer-domain lease."""

    def __init__(self, path: Path, *, exclusive: bool) -> None:
        self.path = path
        self.exclusive = exclusive
        self._file: BinaryIO | None = None

    def acquire(self, timeout: float = WRITER_DOMAIN_ACQUIRE_TIMEOUT_SECONDS) -> None:
        self.path.parent.mkdir(parents=True, exist_ok=True)
        file = self.path.open("a+b")
        if os.name != "nt":
            os.chmod(self.path, 0o600)
        deadline = time.monotonic() + timeout
        while not _try_writer_domain_lock(file, exclusive=self.exclusive):
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                file.close()
                kind = "exclusive sandbox audit" if self.exclusive else "production writer"
                raise WriterDomainOverlap(
                    f"{WRITER_DOMAIN_OVERLAP_CLASSIFICATION}: {kind} could not acquire "
                    f"{self.path} within {timeout:.3f}s"
                )
            time.sleep(min(0.025, remaining))
        self._file = file

    def release(self) -> None:
        if self._file is None:
            return
        try:
            _unlock_writer_domain(self._file)
        finally:
            self._file.close()
            self._file = None


def _find_newer(
    root: Path,
    sentinel_mtime: float,
    pre_existing: set[Path],
) -> list[Path]:
    if not root.exists():
        return []
    offenders: list[Path] = []
    try:
        iterator = root.rglob("*")
    except OSError:
        return offenders
    for path in iterator:
        if path in pre_existing:
            continue
        try:
            stat = path.lstat()
        except OSError:
            continue
        if stat.st_mtime > sentinel_mtime:
            offenders.append(path)
    return offenders


def _otool_dylibs(binary: Path) -> list[Path]:
    if sys.platform != "darwin":
        return []
    try:
        result = subprocess.run(
            ["otool", "-L", str(binary)],
            check=True,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (FileNotFoundError, subprocess.SubprocessError):
        return []

    dylibs: list[Path] = []
    for line in result.stdout.splitlines():
        match = re.match(r"^\s*(@rpath|@loader_path)/(\S+\.dylib)\b", line)
        if not match:
            continue
        candidate = binary.parent / match.group(2)
        if candidate.exists():
            dylibs.append(candidate)
    return dylibs


def resolve_binary(repo_root: Path) -> Path:
    override = os.environ.get("SHIPYARD_BINARY_FOR_TEST")
    candidates = [
        Path(override).expanduser() if override else None,
        repo_root / "target" / "release" / BINARY_NAME,
        repo_root / "target" / "debug" / BINARY_NAME,
    ]
    for candidate in candidates:
        if candidate and candidate.exists():
            return candidate.resolve()
    raise FileNotFoundError(
        "No shipyard binary found. Run `cargo build --release` or set "
        "SHIPYARD_BINARY_FOR_TEST=/path/to/shipyard."
    )


def resolve_python_shipyard_source(repo_root: Path) -> PythonShipyardSource | None:
    if sys.platform == "win32":
        return None

    repo_override = os.environ.get("SHIPYARD_PYTHON_REPO_FOR_TEST")
    repo_candidates = [
        Path(repo_override).expanduser() if repo_override else None,
        repo_root.parent / "shipyard",
    ]
    python_override = os.environ.get("SHIPYARD_PYTHON_FOR_TEST")

    for candidate in repo_candidates:
        if candidate is None:
            continue
        candidate = candidate.resolve()
        if not (candidate / "src" / "shipyard" / "cli.py").exists():
            continue
        python_candidates = [
            Path(python_override).expanduser() if python_override else None,
            candidate / ".venv" / "bin" / "python",
            candidate / ".venv" / "bin" / "python3",
            Path(shutil.which("python3") or ""),
        ]
        for python in python_candidates:
            if python and python.exists() and _python_can_import_shipyard(python, candidate):
                return PythonShipyardSource(repo_root=candidate, python=python.expanduser())
    return None


def _python_can_import_shipyard(python: Path, repo_root: Path) -> bool:
    env = os.environ.copy()
    pythonpath = str(repo_root / "src")
    if env.get("PYTHONPATH"):
        pythonpath = f"{pythonpath}{os.pathsep}{env['PYTHONPATH']}"
    env["PYTHONPATH"] = pythonpath
    try:
        result = subprocess.run(
            [str(python), "-c", "import shipyard.cli"],
            env=env,
            check=False,
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return False
    return result.returncode == 0


class Sandbox:
    def __init__(self, *, keep: bool = False) -> None:
        self.keep = keep
        self.root: Path | None = None
        self._sentinel_mtime: float | None = None
        self._pre_existing: dict[Path, set[Path]] = {}
        self._writer_domain_lease: WriterDomainLease | None = None

    @property
    def bin_dir(self) -> Path:
        return self._require_root() / "bin"

    @property
    def home_dir(self) -> Path:
        return self._require_root() / "home"

    @property
    def work_dir(self) -> Path:
        return self._require_root() / "work"

    @property
    def binary_path(self) -> Path:
        return self.bin_dir / BINARY_NAME

    def setup(self) -> None:
        if self.root is not None:
            raise RuntimeError("sandbox already set up")
        self.root = Path(tempfile.mkdtemp(prefix="shipyard-sandbox-e2e."))
        self.bin_dir.mkdir(parents=True)
        self.home_dir.mkdir(parents=True)
        self.work_dir.mkdir(parents=True)
        lease = WriterDomainLease(production_writer_domain_lock_path(), exclusive=True)
        try:
            lease.acquire()
        except Exception:
            shutil.rmtree(self.root, ignore_errors=True)
            self.root = None
            raise
        self._writer_domain_lease = lease
        self._pre_existing = {path: _snapshot_paths(path) for path in PROTECTED_PATHS}
        sentinel = self.root / ".sentinel"
        sentinel.write_text("sandbox-start\n", encoding="utf-8")
        now = time.time()
        os.utime(sentinel, (now, now))
        self._sentinel_mtime = sentinel.stat().st_mtime

    def teardown(self) -> None:
        if self.root is None:
            return
        try:
            self.assert_no_contamination()
        finally:
            try:
                if not self.keep:
                    shutil.rmtree(self.root, ignore_errors=True)
                self.root = None
            finally:
                if self._writer_domain_lease is not None:
                    self._writer_domain_lease.release()
                    self._writer_domain_lease = None

    def __enter__(self) -> "Sandbox":
        self.setup()
        return self

    def __exit__(self, *exc_info: object) -> None:
        self.teardown()

    def stage_binary(self, binary: Path) -> Path:
        self._require_root()
        staged = self.binary_path
        shutil.copy2(binary, staged)
        staged.chmod(0o755)
        for dylib in _otool_dylibs(binary):
            shutil.copy2(dylib, self.bin_dir / dylib.name)
        return staged

    def stage_python_shipyard(self, source: PythonShipyardSource) -> Path:
        self._require_root()
        script = self.bin_dir / PYTHON_BINARY_NAME
        pythonpath = source.repo_root / "src"
        script.write_text(
            "#!/bin/sh\n"
            f"export PYTHONPATH={shlex.quote(str(pythonpath))}"
            '${PYTHONPATH:+:$PYTHONPATH}\n'
            f"exec {shlex.quote(str(source.python))} -m shipyard.cli \"$@\"\n",
            encoding="utf-8",
        )
        script.chmod(0o755)
        return script

    def run(
        self,
        args: Sequence[str],
        *,
        binary: str = BINARY_NAME,
        timeout: float = 20.0,
        extra_env: Mapping[str, str] | None = None,
    ) -> RunResult:
        self._guard(args)
        env = self._env(extra_env)
        executable = self.bin_dir / binary
        if not executable.exists():
            raise FileNotFoundError(f"binary not staged: {executable}")
        sandbox_args = list(args)
        if binary == BINARY_NAME and "--mode" not in sandbox_args:
            sandbox_args = ["--mode", "isolated", *sandbox_args]
        argv = [str(executable), *sandbox_args]
        result = subprocess.run(
            argv,
            cwd=self.work_dir,
            env=env,
            check=False,
            capture_output=True,
            text=True,
            timeout=timeout,
        )
        return RunResult(
            argv=tuple(argv),
            returncode=result.returncode,
            stdout=result.stdout,
            stderr=result.stderr,
        )

    def assert_no_contamination(self) -> None:
        if self._sentinel_mtime is None:
            raise RuntimeError("sandbox not set up")
        offenders: list[Path] = []
        for path in PROTECTED_PATHS:
            offenders.extend(
                _find_newer(
                    path,
                    self._sentinel_mtime,
                    self._pre_existing.get(path, set()),
                )
            )
        report = ContaminationReport(tuple(sorted(offenders)), self._sentinel_mtime)
        if not report.clean:
            raise AssertionError(report.format())

    def _env(self, extra_env: Mapping[str, str] | None) -> dict[str, str]:
        path_entries = [str(self.bin_dir), "/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        env = {
            "HOME": str(self.home_dir),
            "USERPROFILE": str(self.home_dir),
            "PATH": os.pathsep.join(path_entries),
            "XDG_CONFIG_HOME": str(self.home_dir / ".config"),
            "XDG_STATE_HOME": str(self.home_dir / ".local" / "state"),
            "XDG_CACHE_HOME": str(self.home_dir / ".cache"),
            "RUST_BACKTRACE": "1",
        }
        if extra_env:
            env.update(extra_env)
        return env

    def _guard(self, args: Sequence[str]) -> None:
        command = _first_command(args)
        if command is None:
            return
        if "--help" in args or "-h" in args or "help" in command:
            return
        first = command[0]
        if first in DESTRUCTIVE_TOP_LEVEL:
            raise AssertionError(f"refusing destructive command: {first}")
        if first == "daemon" and len(command) > 1 and command[1] in DESTRUCTIVE_DAEMON:
            raise AssertionError(f"refusing daemon mutation command: {' '.join(command[:2])}")
        if first == "cloud" and self._cloud_is_destructive(tuple(command[1:])):
            raise AssertionError(f"refusing cloud mutation command: {' '.join(command)}")
        if first == "watch" and "--no-follow" not in command:
            raise AssertionError("refusing watch without --no-follow")

    def _cloud_is_destructive(self, rest: tuple[str, ...]) -> bool:
        for destructive in DESTRUCTIVE_CLOUD:
            if rest[: len(destructive)] == destructive:
                return True
        return False

    def _require_root(self) -> Path:
        if self.root is None:
            raise RuntimeError("sandbox not set up")
        return self.root


def _first_command(args: Sequence[str]) -> tuple[str, ...] | None:
    args = tuple(args)
    value_options = {"--mode", "--state-dir", "--global-dir", "--cwd"}
    index = 0
    while index < len(args):
        arg = args[index]
        if arg in value_options:
            index += 2
            continue
        if any(arg.startswith(f"{option}=") for option in value_options):
            index += 1
            continue
        if arg == "--json":
            index += 1
            continue
        if arg.startswith("-"):
            index += 1
            continue
        return args[index:]
    return None


_COMMAND_LINE_RE = re.compile(r"^\s{2}([A-Za-z0-9][A-Za-z0-9_-]*)(?:\s{2,}|\s*$)")


def parse_advertised_commands(help_text: str) -> set[str]:
    """Return subcommands advertised in a Clap-style help `Commands:` block."""
    commands: set[str] = set()
    in_commands = False
    for line in help_text.splitlines():
        if line.strip() == "Commands:":
            in_commands = True
            continue
        if in_commands and not line.strip():
            break
        if not in_commands:
            continue
        stripped = line.strip()
        if not stripped:
            continue
        match = _COMMAND_LINE_RE.match(line)
        if match:
            commands.add(match.group(1))
    return commands


def parse_top_level_commands(help_text: str) -> set[str]:
    return parse_advertised_commands(help_text)
