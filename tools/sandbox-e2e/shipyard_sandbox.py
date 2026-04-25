"""Core harness for the Shipyard sandbox E2E test suite.

Every test runs in an isolated tempdir + shadowed ``$PATH`` + isolated
``$HOME``. The harness MUST never write to the user's real Shipyard
install. ``Sandbox.assert_no_contamination()`` is invoked at teardown
by the pytest ``sandbox`` fixture and must pass after every scenario.

Design constraints (see ``Shipyard #248`` for the full spec, plus the
sibling Pulp implementation at
`pulp#732 <https://github.com/danielraffel/pulp/issues/732>`_):

* ``$PATH`` is shadowed to exactly ``$SANDBOX/bin:/usr/bin:/bin`` so a
  stray ``shipyard`` elsewhere on ``PATH`` cannot be invoked
  accidentally.
* ``$HOME`` is set to ``$SANDBOX/home`` for every subprocess so all
  ``Path.home()``-derived locations Shipyard touches
  (``~/Library/Application Support/shipyard/`` on macOS,
  ``~/.config/shipyard/`` on Linux, ``~/.local/state/shipyard/`` on
  Linux, ``~/AppData/Local/shipyard/`` on Windows, plus the
  ``~/.local/bin/shipyard`` install location) become sandbox-local.
* Binaries are *copied* (not symlinked) into ``$SANDBOX/bin``. The
  harness inspects ``otool -L`` (macOS) for any ``@rpath`` /
  ``@loader_path`` dylibs and copies them alongside. The current
  Shipyard PyInstaller binary is statically linked against system
  libs, so this is a no-op today — but it's the same primitive Pulp's
  C++ binary needs and we want both harnesses to look identical.
* The sandbox NEVER invokes ``shipyard ship``, ``shipyard pr``,
  ``shipyard cloud run``, or ``shipyard upgrade``. Those are listed
  in ``DESTRUCTIVE_COMMANDS`` and the README.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Mapping, Sequence


# ----- Contamination audit --------------------------------------------------

#: Directories that must never be written to by any sandbox test.
#: The audit records mtimes at sandbox setup; any file ``newer`` than
#: the sandbox-start sentinel inside these trees fails the teardown.
#:
#: Shipyard's state lives in two places per platform:
#:   * macOS:   ``~/Library/Application Support/shipyard/``
#:   * Linux:   ``~/.config/shipyard/`` (config) +
#:              ``~/.local/state/shipyard/`` (state, daemon socket)
#:   * Windows: ``~/AppData/Local/shipyard/``
#: Plus the install location ``~/.local/bin/shipyard`` (and the ``sy``
#: symlink).
PROTECTED_PATHS: tuple[Path, ...] = (
    # macOS combined config/state dir
    Path.home() / "Library" / "Application Support" / "shipyard",
    # Linux split config / state dirs
    Path.home() / ".config" / "shipyard",
    Path.home() / ".local" / "state" / "shipyard",
    # Windows combined dir (harmless on macOS/Linux — just won't exist)
    Path.home() / "AppData" / "Local" / "shipyard",
    # Install location (and the ``sy`` symlink lives in the same dir)
    Path.home() / ".local" / "bin",
    # Legacy / future-proofing — if anything ever lands a
    # ``~/.shipyard/`` dir we want the audit to flag it loudly.
    Path.home() / ".shipyard",
    Path.home() / ".cache" / "shipyard",
)


#: Subcommands the sandbox MUST NOT invoke. Documented in README.md.
#: A test that names one of these gets a clear AssertionError instead
#: of silently scribbling on the user's repo or remote infra.
DESTRUCTIVE_COMMANDS: frozenset[str] = frozenset({
    "ship",      # actually merges PRs!
    "pr",        # opens a real GitHub PR
    "upgrade",   # replaces the running binary
    # `cloud run` is destructive (dispatches a real GHA workflow); the
    # parent `cloud` group has read-only subcommands so we don't blanket
    # ban it — see SAFE_CLOUD_SUBCOMMANDS below for the allowlist.
})

#: Whitelist of ``shipyard cloud …`` subcommands that are safe to
#: invoke — they're read-only or ``--help``-shaped.
SAFE_CLOUD_SUBCOMMANDS: frozenset[str] = frozenset({
    "workflows",   # read-only enumeration
    "defaults",    # read-only config dump
    "status",      # read-only status fetch
    "--help",
})


@dataclass(frozen=True)
class ContaminationReport:
    """What the contamination audit found, if anything."""

    offenders: tuple[Path, ...]
    sentinel_mtime: float

    @property
    def clean(self) -> bool:
        return not self.offenders

    def format(self) -> str:
        if self.clean:
            return "clean — no writes to protected paths"
        lines = [
            "CONTAMINATION DETECTED — test wrote outside its sandbox:",
        ]
        lines.extend(f"  {p}" for p in self.offenders)
        lines.append(f"(sentinel mtime: {self.sentinel_mtime})")
        return "\n".join(lines)


def _snapshot_paths(root: Path) -> set[Path]:
    """Snapshot the set of paths currently under ``root``. Used at
    sandbox setup so the audit at teardown can flag genuinely-new
    files without false-positives from unrelated processes (e.g. a
    live ``shipyard daemon`` updating its own state dir while the
    sandbox is running)."""
    if not root.exists():
        return set()
    try:
        return set(root.rglob("*"))
    except OSError:
        return set()


def _find_newer(
    root: Path,
    sentinel_mtime: float,
    *,
    pre_existing: set[Path] | None = None,
) -> list[Path]:
    """Return paths under ``root`` that look like contamination — that
    is, *new* paths (not in ``pre_existing``) whose mtime is strictly
    greater than ``sentinel_mtime``. Skips missing roots. Never
    follows symlinks.

    Why ``pre_existing`` matters: a developer running the suite locally
    may have a live ``shipyard daemon`` writing to its own state dir
    independently of the sandbox. Treating any mtime bump in that dir
    as contamination would produce false positives. Only files the
    sandbox could plausibly have created (i.e. paths absent at
    sandbox setup) count as offenders.
    """
    if not root.exists():
        return []
    newer: list[Path] = []
    try:
        iterator = root.rglob("*")
    except OSError:
        return []
    pre = pre_existing or set()
    for path in iterator:
        if path in pre:
            continue  # daemon-managed file we knew about — not us
        try:
            st = path.lstat()
        except (OSError, FileNotFoundError):
            continue
        if st.st_mtime > sentinel_mtime:
            newer.append(path)
    return newer


# ----- Binary staging -------------------------------------------------------


def _otool_dylibs(binary: Path) -> list[Path]:
    """Return ``@rpath`` / ``@loader_path`` dylibs that must sit next to
    the binary for the rpath contract. Returns ``[]`` on non-macOS or
    on an ``otool`` error.

    Today the Shipyard PyInstaller binary statically links against
    system libs so this returns ``[]`` for it. Kept as a primitive so
    the harness shape stays parallel to Pulp's and so we're ready when
    a future binary picks up an ``@rpath`` dependency."""
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
        line = line.strip()
        m = re.match(r"^(@rpath|@loader_path)/(\S+\.dylib)\b", line)
        if not m:
            continue
        candidate = binary.parent / m.group(2)
        if candidate.exists():
            dylibs.append(candidate)
    return dylibs


# ----- Sandbox --------------------------------------------------------------


@dataclass
class RunResult:
    """Captured result of a ``Sandbox.run`` invocation."""

    argv: tuple[str, ...]
    returncode: int
    stdout: str
    stderr: str

    def expect_success(self) -> "RunResult":
        if self.returncode != 0:
            raise AssertionError(
                f"command failed (rc={self.returncode}): {' '.join(self.argv)}\n"
                f"stdout: {self.stdout}\nstderr: {self.stderr}"
            )
        return self


class Sandbox:
    """Isolated sandbox for running the ``shipyard`` CLI under test.

    Usage::

        with Sandbox() as sbx:
            sbx.stage_binary(Path("/.../shipyard"), as_name="shipyard")
            sbx.run(["--version"]).expect_success()
            sbx.assert_no_contamination()
    """

    def __init__(self, *, keep: bool = False) -> None:
        self._keep = keep
        self._tmpdir: Path | None = None
        self._sentinel_mtime: float | None = None
        self._pre_existing: dict[Path, set[Path]] = {}

    # -- lifecycle --

    def __enter__(self) -> "Sandbox":
        self.setup()
        return self

    def __exit__(self, *exc_info) -> None:
        self.teardown()

    def setup(self) -> None:
        if self._tmpdir is not None:
            raise RuntimeError("Sandbox already set up")
        self._tmpdir = Path(tempfile.mkdtemp(prefix="shipyard-sandbox-e2e."))
        (self._tmpdir / "bin").mkdir()
        (self._tmpdir / "home").mkdir()
        # The fake $HOME needs the dirs Shipyard might write to so the
        # OS-native path resolution succeeds without surprising the
        # CLI. We don't create the dirs themselves; Shipyard does that
        # on first write.
        sentinel = self._tmpdir / ".sentinel"
        sentinel.write_text("")
        self._sentinel_mtime = sentinel.stat().st_mtime
        # Snapshot the protected paths NOW so the audit at teardown can
        # distinguish "the test wrote a new file here" from "an
        # unrelated daemon updated a pre-existing file in its own
        # state dir." We only flag paths absent at this snapshot.
        self._pre_existing = {
            root: _snapshot_paths(root) for root in PROTECTED_PATHS
        }
        # Force post-sentinel writes to have strictly greater mtime on
        # 1s-resolution filesystems (macOS HFS+ / FAT).
        time.sleep(0.05)

    def teardown(self) -> None:
        if self._tmpdir is None:
            return
        if not self._keep:
            shutil.rmtree(self._tmpdir, ignore_errors=True)
        self._tmpdir = None
        self._sentinel_mtime = None

    # -- accessors --

    @property
    def root(self) -> Path:
        assert self._tmpdir is not None, "sandbox not set up"
        return self._tmpdir

    @property
    def bin_dir(self) -> Path:
        return self.root / "bin"

    @property
    def home(self) -> Path:
        return self.root / "home"

    @property
    def sentinel_mtime(self) -> float:
        assert self._sentinel_mtime is not None
        return self._sentinel_mtime

    # -- staging --

    def stage_binary(self, source: Path, as_name: str) -> Path:
        """Copy ``source`` into ``$SANDBOX/bin/<as_name>`` along with
        any ``@rpath`` / ``@loader_path`` dylibs it needs. Returns the
        staged path.

        Copies (not symlinks) are mandatory — the harness must never
        give a test a back-door to the real installed binary.
        """
        source = source.resolve()
        if not source.is_file():
            raise FileNotFoundError(f"binary not found: {source}")
        dest = self.bin_dir / as_name
        shutil.copy2(source, dest)
        dest.chmod(0o755)
        for dylib in _otool_dylibs(source):
            dylib_dest = self.bin_dir / dylib.name
            if not dylib_dest.exists():
                shutil.copy2(dylib, dylib_dest)
        return dest

    def stage_python_wrapper(self, repo_root: Path, as_name: str = "shipyard") -> Path:
        """Stage a wrapper that invokes ``python -m shipyard.cli``
        from the in-repo source tree. Used when no PyInstaller binary
        is available (typical CI fallback when the release build hasn't
        run yet on this branch).

        The wrapper sets ``PYTHONPATH`` to the repo's ``src/`` dir so
        the in-repo Shipyard package is what gets executed — *not*
        whatever ``shipyard`` happens to be installed in the user's
        site-packages. That's the same isolation contract the binary
        path gets.
        """
        src_dir = (repo_root / "src").resolve()
        if not src_dir.is_dir():
            raise FileNotFoundError(
                f"expected repo src/ at {src_dir} for python-wrapper "
                f"fallback"
            )
        dest = self.bin_dir / as_name
        # Use the same python that's running pytest. ``sys.executable``
        # is correct in any venv / system-python combination.
        body = (
            f"#!/usr/bin/env bash\n"
            f"# Auto-generated by Sandbox.stage_python_wrapper.\n"
            f"# Routes `shipyard` to the in-repo source tree at\n"
            f"# {src_dir}\n"
            f"export PYTHONPATH={src_dir}:${{PYTHONPATH:-}}\n"
            f'exec "{sys.executable}" -m shipyard.cli "$@"\n'
        )
        dest.write_text(body)
        dest.chmod(0o755)
        return dest

    def write_stub(self, name: str, body: str) -> Path:
        """Write an executable shell stub to ``$SANDBOX/bin/<name>``."""
        dest = self.bin_dir / name
        dest.write_text(body)
        dest.chmod(0o755)
        return dest

    # -- execution --

    def env(self, overrides: Mapping[str, str] | None = None) -> dict[str, str]:
        """Minimal environment for subprocess invocation.

        ``PATH`` is shadowed to ``$SANDBOX/bin:/usr/bin:/bin`` so any
        stray ``shipyard`` on the user's ``PATH`` cannot be invoked.
        ``HOME`` points at the sandbox's isolated state dir so every
        ``Path.home()`` resolution inside Shipyard lands inside the
        sandbox.
        """
        base: dict[str, str] = {
            "PATH": f"{self.bin_dir}:/usr/bin:/bin",
            "HOME": str(self.home),
            "LANG": os.environ.get("LANG", "C"),
            "LC_ALL": os.environ.get("LC_ALL", "C"),
            "TERM": "dumb",
        }
        # Carry through env that subprocesses (gh, ssh, git) need.
        # SHELL lets click figure out completion shells; TMPDIR keeps
        # macOS subprocess scratch space sane.
        for k in ("USER", "LOGNAME", "TMPDIR", "SHELL"):
            if k in os.environ:
                base[k] = os.environ[k]
        if overrides:
            base.update(overrides)
        return base

    def run(
        self,
        argv: Sequence[str],
        *,
        binary: str = "shipyard",
        env_overrides: Mapping[str, str] | None = None,
        cwd: Path | None = None,
        timeout: float = 60.0,
    ) -> RunResult:
        """Invoke ``$SANDBOX/bin/<binary>`` with ``argv`` under the
        sandbox environment.

        The default timeout is generous (60s) because PyInstaller
        ``--onefile`` binaries on macOS have a 5-8s cold-start
        penalty. Tests that want to flag a hang faster can pass an
        explicit shorter ``timeout``.

        Refuses to invoke any subcommand listed in
        ``DESTRUCTIVE_COMMANDS`` — those mutate real PRs, real
        binaries, or real cloud runs. Use ``--help`` instead, or shell
        out to a stub.

        Returns a ``RunResult`` with captured stdout/stderr/returncode.
        Raises ``TimeoutError`` if the child outruns ``timeout``.
        """
        if argv and argv[0] in DESTRUCTIVE_COMMANDS:
            raise AssertionError(
                f"refusing to invoke destructive subcommand "
                f"{argv[0]!r} from sandbox; see DESTRUCTIVE_COMMANDS "
                f"and the README's exclusion table."
            )
        # `cloud` is half-destructive — block its destructive children.
        if (
            len(argv) >= 2
            and argv[0] == "cloud"
            and argv[1] not in SAFE_CLOUD_SUBCOMMANDS
            and argv[1] != "--help"
        ):
            raise AssertionError(
                f"refusing to invoke `cloud {argv[1]}` from sandbox; "
                f"only {sorted(SAFE_CLOUD_SUBCOMMANDS)} are allowed."
            )
        exe = self.bin_dir / binary
        if not exe.exists():
            raise FileNotFoundError(
                f"binary '{binary}' not staged in {self.bin_dir}"
            )
        full_argv = (str(exe),) + tuple(argv)
        try:
            proc = subprocess.run(
                full_argv,
                env=self.env(env_overrides),
                cwd=str(cwd) if cwd else str(self.root),
                capture_output=True,
                text=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as exc:
            raise TimeoutError(
                f"timeout running {' '.join(full_argv)} after {timeout}s\n"
                f"partial stdout: {exc.stdout}\nstderr: {exc.stderr}"
            ) from exc
        return RunResult(
            argv=full_argv,
            returncode=proc.returncode,
            stdout=proc.stdout or "",
            stderr=proc.stderr or "",
        )

    # -- state readback --

    def read(self, rel_path: str) -> str:
        """Read a file under ``$SANDBOX/home``. Raises
        ``FileNotFoundError`` if missing."""
        return (self.home / rel_path).read_text()

    def exists(self, rel_path: str) -> bool:
        return (self.home / rel_path).exists()

    # -- contamination audit --

    def audit_contamination(
        self,
        extra_roots: Iterable[Path] = (),
    ) -> ContaminationReport:
        """Scan the protected paths (plus any ``extra_roots``) for
        contamination. A path counts as an offender when:

        * its mtime is strictly greater than the sandbox-start
          sentinel, AND
        * it did NOT exist at sandbox setup time

        The "did not exist" filter screens out unrelated processes
        (e.g. a live ``shipyard daemon``) updating pre-existing files
        in their own state dirs. ``extra_roots`` injected after setup
        get a fresh empty snapshot — useful for the audit's own self-
        test (see ``test_contamination_audit_catches_writes_to_protected_paths``)."""
        offenders: list[Path] = []
        for root in PROTECTED_PATHS:
            offenders.extend(
                _find_newer(
                    root,
                    self.sentinel_mtime,
                    pre_existing=self._pre_existing.get(root, set()),
                )
            )
        for root in extra_roots:
            offenders.extend(
                _find_newer(root, self.sentinel_mtime, pre_existing=set())
            )
        return ContaminationReport(
            offenders=tuple(offenders),
            sentinel_mtime=self.sentinel_mtime,
        )

    def assert_no_contamination(self) -> None:
        """Raise ``AssertionError`` if the audit finds any writes to
        protected paths since the sandbox was set up."""
        report = self.audit_contamination()
        if not report.clean:
            raise AssertionError(report.format())


# ----- Surface enumeration --------------------------------------------------

#: Matches ``shipyard <word>`` at start of line, after whitespace, or
#: inside a fenced code block / inline backticks. Captures the first
#: subcommand token. Mirrors the Pulp harness's ``_PLUGIN_CMD_RE`` so
#: a contributor reading both finds the same shape.
_SHIPYARD_CMD_RE = re.compile(
    r"""(?:^|\s|`)              # start, whitespace, or backtick
        shipyard\s+              # the binary
        ([a-z][a-z-]*)           # subcommand token (no flags)
    """,
    re.VERBOSE | re.MULTILINE,
)


def enumerate_shipyard_commands(roots: Iterable[Path]) -> set[str]:
    """Parse every Markdown / YAML file under ``roots`` and return the
    set of ``shipyard <subcommand>`` invocations the surface advertises.

    Surfaces worth scanning (per the design refinement #1 in
    `Shipyard #248
    <https://github.com/danielraffel/Shipyard/issues/248#issuecomment-4315974191>`_):

    * ``commands/*.md`` — the Claude Code plugin's slash-command files
    * ``README.md`` — fenced-block examples
    * ``docs/**/*.md`` — guide pages with example invocations
    * ``.github/workflows/*.yml`` — CI workflows that shell out

    Tokens that are obvious docs noise (``the``, ``and``, ``is``, plus
    the angle-bracketed placeholder ``$arguments``) are stripped.
    """
    subcommands: set[str] = set()
    for root in roots:
        if not root.exists():
            continue
        if root.is_file():
            files = [root]
        else:
            files = []
            for pattern in ("*.md", "**/*.md", "*.yml", "**/*.yml", "*.yaml"):
                files.extend(root.glob(pattern))
        for f in files:
            try:
                text = f.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            for match in _SHIPYARD_CMD_RE.finditer(text):
                subcommands.add(match.group(1))
    # Strip prose-noise tokens that the regex catches in narrative text.
    for noise in ("the", "and", "is", "or", "a", "an"):
        subcommands.discard(noise)
    return subcommands


# ----- JSON-schema parity probe --------------------------------------------


def parse_status_json(stdout: str) -> dict:
    """Parse ``shipyard --json status`` output. Strips any leading
    non-JSON garbage and returns the decoded object."""
    idx = stdout.find("{")
    if idx < 0:
        raise ValueError(f"no JSON object in output: {stdout!r}")
    decoder = json.JSONDecoder()
    obj, _end = decoder.raw_decode(stdout[idx:])
    if not isinstance(obj, dict):
        raise ValueError(f"expected object, got {type(obj).__name__}")
    return obj


#: Top-level keys ``shipyard --json status`` exposes today. Drift in
#: this set is a release-notes-worthy schema change. The parity test
#: asserts these are present so a refactor that drops one fails loudly.
REQUIRED_STATUS_KEYS: tuple[str, ...] = (
    "schema_version",
    "command",
    "queue",
    "targets",
)
