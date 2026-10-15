import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from shipyard_sandbox import (
    CONCURRENT_GUARD_LOGS,
    SANDBOX_IDENTITY_ENVS,
    Sandbox,
    PROTECTED_PATHS,
    SESSION_INBOX_DIR,
    WRITER_DOMAIN_LOCK_NAME,
    WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
    WRITER_DOMAIN_TURNSTILE_NAME,
    WriterDomainLease,
    WriterDomainOverlap,
    _find_newer,
    _snapshot_paths,
    contamination,
    production_writer_domain_lock_path,
    production_writer_domain_turnstile_path,
)


def test_production_writer_domain_lock_is_under_machine_state() -> None:
    home = Path("/host-home")
    path = production_writer_domain_lock_path(home)

    assert path.name == WRITER_DOMAIN_LOCK_NAME
    assert "shipyard" in path.parts
    assert path.is_relative_to(home)
    assert production_writer_domain_turnstile_path(home).name == (
        WRITER_DOMAIN_TURNSTILE_NAME
    )
    assert production_writer_domain_turnstile_path(home).parent == path.parent


def test_exclusive_sandbox_audit_rejects_overlapping_writer(tmp_path: Path) -> None:
    path = tmp_path / WRITER_DOMAIN_LOCK_NAME
    writer = WriterDomainLease(path, exclusive=False)
    audit = WriterDomainLease(path, exclusive=True)
    writer.acquire(timeout=0.05)

    try:
        with pytest.raises(WriterDomainOverlap) as captured:
            audit.acquire(timeout=0.05)
        assert str(captured.value).startswith(WRITER_DOMAIN_OVERLAP_CLASSIFICATION)
        assert "exclusive sandbox audit" in str(captured.value)
    finally:
        writer.release()


def test_shared_writer_rejects_overlapping_sandbox_audit(tmp_path: Path) -> None:
    path = tmp_path / WRITER_DOMAIN_LOCK_NAME
    audit = WriterDomainLease(path, exclusive=True)
    writer = WriterDomainLease(path, exclusive=False)
    audit.acquire(timeout=0.05)

    try:
        with pytest.raises(WriterDomainOverlap) as captured:
            writer.acquire(timeout=0.05)
        assert str(captured.value).startswith(WRITER_DOMAIN_OVERLAP_CLASSIFICATION)
        assert "writer-domain turnstile" in str(captured.value)
    finally:
        audit.release()


def test_multiple_production_writers_can_share_domain(tmp_path: Path) -> None:
    path = tmp_path / WRITER_DOMAIN_LOCK_NAME
    first = WriterDomainLease(path, exclusive=False)
    second = WriterDomainLease(path, exclusive=False)

    first.acquire(timeout=0.05)
    try:
        second.acquire(timeout=0.05)
        second.release()
    finally:
        first.release()


def test_read_only_production_command_coexists_with_exclusive_audit(
    tmp_path: Path, shipyard_binary: Path
) -> None:
    path = production_writer_domain_lock_path(tmp_path)
    audit = WriterDomainLease(path, exclusive=True)
    audit.acquire(timeout=0.05)
    env = os.environ.copy()
    env.update({"HOME": str(tmp_path), "USERPROFILE": str(tmp_path)})

    try:
        result = subprocess.run(
            [str(shipyard_binary), "--mode", "shipyard", "paths"],
            env=env,
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
    finally:
        audit.release()

    assert result.returncode == 0
    assert result.stderr == ""
    assert "state_dir" in result.stdout


def test_existing_queue_read_coexists_with_exclusive_audit(
    tmp_path: Path, shipyard_binary: Path
) -> None:
    path = production_writer_domain_lock_path(tmp_path)
    env = os.environ.copy()
    env.update({"HOME": str(tmp_path), "USERPROFILE": str(tmp_path)})
    command = [str(shipyard_binary), "--mode", "shipyard", "queue"]

    prewarm = subprocess.run(
        command,
        env=env,
        check=False,
        capture_output=True,
        text=True,
        timeout=5,
    )
    assert prewarm.returncode == 0, prewarm.stderr

    audit = WriterDomainLease(path, exclusive=True)
    audit.acquire(timeout=0.05)
    try:
        result = subprocess.run(
            command,
            env=env,
            check=False,
            capture_output=True,
            text=True,
            timeout=5,
        )
    finally:
        audit.release()

    assert result.returncode == 0, result.stderr
    assert result.stderr == ""
    assert "No pending jobs" in result.stdout


def test_production_mutation_waits_then_resumes_after_audit(
    tmp_path: Path, shipyard_binary: Path
) -> None:
    path = production_writer_domain_lock_path(tmp_path)
    audit = WriterDomainLease(path, exclusive=True)
    audit.acquire(timeout=0.05)
    env = os.environ.copy()
    env.update({"HOME": str(tmp_path), "USERPROFILE": str(tmp_path)})
    process = subprocess.Popen(
        [
            str(shipyard_binary),
            "--mode",
            "shipyard",
            "metrics",
            "record",
            "--project",
            "writer-domain-test",
            "--job",
            "mutation",
            "--duration-ms",
            "1",
        ],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )

    try:
        time.sleep(0.15)
        assert process.poll() is None
        created = {
            candidate.relative_to(path.parent)
            for candidate in path.parent.rglob("*")
        }
        assert created == {
            Path(WRITER_DOMAIN_LOCK_NAME),
            Path(WRITER_DOMAIN_TURNSTILE_NAME),
        }
    finally:
        audit.release()

    stdout, stderr = process.communicate(timeout=10)
    assert process.returncode == 0, stderr
    assert stderr == ""
    assert "recorded job" in stdout
    assert (path.parent / "metrics" / "metrics.db").is_file()


@pytest.mark.skipif(sys.platform == "win32", reason="daemon IPC is Unix-only")
def test_idle_daemon_does_not_hold_writer_domain(
    shipyard_binary: Path,
) -> None:
    with tempfile.TemporaryDirectory(prefix="sy-wd-", dir="/tmp") as home:
        path = production_writer_domain_lock_path(Path(home))
        env = os.environ.copy()
        env.update(
            {
                "HOME": home,
                "USERPROFILE": home,
                "SHIPYARD_ENABLE_TUNNEL": "0",
            }
        )
        daemon = subprocess.Popen(
            [str(shipyard_binary), "--mode", "shipyard", "daemon", "run"],
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )

        try:
            deadline = time.monotonic() + 10
            status = None
            while time.monotonic() < deadline:
                status = subprocess.run(
                    [str(shipyard_binary), "--mode", "shipyard", "daemon", "status"],
                    env=env,
                    check=False,
                    capture_output=True,
                    text=True,
                    timeout=2,
                )
                if status.returncode == 0 and "daemon running" in status.stdout:
                    break
                assert daemon.poll() is None, daemon.stderr.read()
                time.sleep(0.05)
            else:
                pytest.fail(f"daemon never became ready: {status}")

            audit = WriterDomainLease(path, exclusive=True)
            audit.acquire(timeout=1.0)
            try:
                assert daemon.poll() is None
                before = {
                    candidate.relative_to(path.parent): (
                        candidate.lstat().st_mode,
                        candidate.lstat().st_size,
                        candidate.lstat().st_mtime_ns,
                    )
                    for candidate in path.parent.rglob("*")
                }
                time.sleep(0.2)
                after = {
                    candidate.relative_to(path.parent): (
                        candidate.lstat().st_mode,
                        candidate.lstat().st_size,
                        candidate.lstat().st_mtime_ns,
                    )
                    for candidate in path.parent.rglob("*")
                }
                assert after == before
            finally:
                audit.release()
        finally:
            subprocess.run(
                [str(shipyard_binary), "--mode", "shipyard", "daemon", "stop"],
                env=env,
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            try:
                daemon.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon.terminate()
                daemon.wait(timeout=5)

        stderr = daemon.stderr.read()
        assert daemon.returncode == 0, stderr


def test_python_and_binary_resolve_the_same_writer_domain(
    tmp_path: Path, shipyard_binary: Path
) -> None:
    env = os.environ.copy()
    env.update({"HOME": str(tmp_path), "USERPROFILE": str(tmp_path)})

    result = subprocess.run(
        [str(shipyard_binary), "--mode", "shipyard", "--json", "paths"],
        env=env,
        check=True,
        capture_output=True,
        text=True,
        timeout=5,
    )
    payload = json.loads(result.stdout)

    assert production_writer_domain_lock_path(tmp_path).parent == Path(
        payload["state_dir"]
    )


@pytest.mark.skipif(os.name == "nt", reason="parent SIGTERM proof uses POSIX process semantics")
@pytest.mark.parametrize("surface", ["updater", "prepush"])
def test_guarded_external_writer_retains_lease_after_parent_death(
    tmp_path: Path, shipyard_binary: Path, surface: str
) -> None:
    home = tmp_path / "home"
    protected = (
        home / ".local/bin/shipyard-next"
        if surface == "updater"
        else home / ".local/state/shipyard/changed-surface-prepush/result.json"
    )
    protected.parent.mkdir(parents=True)
    ready = tmp_path / f"{surface}.ready"
    go = tmp_path / f"{surface}.go"
    completed = tmp_path / f"{surface}.completed"
    child_pid = tmp_path / f"{surface}.child-pid"
    guardian_pid = tmp_path / f"{surface}.guardian-pid"
    env = {**os.environ, "HOME": str(home), "USERPROFILE": str(home)}
    parent = os.fork()
    if parent == 0:
        try:
            guardian_process = subprocess.Popen(
                [
                    str(shipyard_binary),
                    "writer-domain-exec",
                    "--path",
                    str(protected),
                    "--",
                    sys.executable,
                    "-c",
                    (
                        "import os,pathlib,sys,time\n"
                        "ready,go,completed,output,child_pid=map(pathlib.Path,sys.argv[1:])\n"
                        "child_pid.write_text(str(os.getpid()),encoding='utf-8')\n"
                        "ready.touch()\n"
                        "while not go.exists():\n"
                        "    time.sleep(0.01)\n"
                        "output.write_text('guarded',encoding='utf-8')\n"
                        "completed.touch()\n"
                    ),
                    str(ready),
                    str(go),
                    str(completed),
                    str(protected),
                    str(child_pid),
                ],
                env=env,
                start_new_session=True,
            )
            guardian_pid.write_text(str(guardian_process.pid), encoding="utf-8")
            return_code = guardian_process.wait()
        except BaseException:
            return_code = 1
        os._exit(return_code)

    guardian: int | None = None
    lease_observed = False
    audit_acquired = False
    normal_complete = False
    audit = WriterDomainLease(production_writer_domain_lock_path(home), exclusive=True)
    try:
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            waited, _ = os.waitpid(parent, os.WNOHANG)
            assert waited == 0
            if guardian_pid.exists():
                guardian = int(guardian_pid.read_text(encoding="utf-8"))
                os.kill(guardian, 0)
                try:
                    audit.acquire(timeout=0.05)
                except WriterDomainOverlap:
                    lease_observed = True
                    if ready.exists():
                        break
                else:
                    audit.release()
            time.sleep(0.01)
        assert lease_observed
        assert ready.exists()

        try:
            os.kill(parent, signal.SIGTERM)
        except ProcessLookupError:
            pass
        os.waitpid(parent, 0)
        parent = 0

        assert guardian is not None
        os.kill(guardian, 0)
        with pytest.raises(WriterDomainOverlap):
            audit.acquire(timeout=0.1)
        assert not completed.exists()
        assert not protected.exists()

        go.touch()
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            if completed.exists():
                try:
                    audit.acquire(timeout=0.05)
                except WriterDomainOverlap:
                    pass
                else:
                    audit_acquired = True
                    break
            time.sleep(0.01)
        else:
            pytest.fail("writer-domain guardian did not release after child completion")
        assert completed.exists()
        assert protected.read_text(encoding="utf-8") == "guarded"
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                os.kill(guardian, 0)
            except ProcessLookupError:
                guardian = None
                normal_complete = True
                break
            time.sleep(0.01)
    finally:
        if audit_acquired:
            audit.release()
        if parent:
            try:
                os.kill(parent, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                os.waitpid(parent, 0)
            except ChildProcessError:
                pass
        if guardian is not None and not normal_complete:
            try:
                os.killpg(guardian, signal.SIGTERM)
            except (PermissionError, ProcessLookupError):
                pass
            deadline = time.monotonic() + 1
            while time.monotonic() < deadline:
                try:
                    if child_pid.exists():
                        child = int(child_pid.read_text(encoding="utf-8"))
                        if os.getpgid(child) != guardian:
                            break
                    else:
                        os.killpg(guardian, 0)
                except (PermissionError, ProcessLookupError, ValueError):
                    break
                time.sleep(0.01)
            else:
                try:
                    os.killpg(guardian, signal.SIGKILL)
                except (PermissionError, ProcessLookupError):
                    pass


def test_protected_write_is_never_allowlisted_by_filename(tmp_path: Path) -> None:
    sentinel_mtime = 1_000.0
    queue_temp = tmp_path / ".queue-85829-1785087066142925000-0.json.tmp"
    outcome = tmp_path / "queue" / "outcomes" / "sy-existing.json"
    outcome.parent.mkdir(parents=True)
    queue_temp.write_text("temp", encoding="utf-8")
    outcome.write_text("{}", encoding="utf-8")

    offenders = _find_newer(tmp_path, sentinel_mtime, {})

    assert queue_temp in offenders
    assert outcome in offenders


def test_transient_directory_entry_does_not_contaminate_unchanged_tree(
    tmp_path: Path,
) -> None:
    protected = tmp_path / "state"
    protected.mkdir()
    before = _snapshot_paths(protected)

    transient = protected / "transient"
    transient.write_text("temporary", encoding="utf-8")
    transient.unlink()

    assert _find_newer(protected, 0.0, before) == []


def test_persistent_directory_entry_is_reported_after_metadata_normalization(
    tmp_path: Path,
) -> None:
    protected = tmp_path / "state"
    protected.mkdir()
    before = _snapshot_paths(protected)

    persistent = protected / "persistent"
    persistent.write_text("outside write", encoding="utf-8")

    assert _find_newer(protected, 0.0, before) == [persistent]


@pytest.mark.parametrize("mutation", ["same-length", "append", "replace", "delete"])
def test_preexisting_protected_file_mutation_is_reported(
    tmp_path: Path, mutation: str
) -> None:
    protected = tmp_path / "state"
    protected.mkdir()
    file = protected / "queue.json"
    file.write_text("AAAA", encoding="utf-8")
    before = _snapshot_paths(protected)

    if mutation == "same-length":
        file.write_text("BBBB", encoding="utf-8")
    elif mutation == "append":
        file.write_text("AAAA-more", encoding="utf-8")
    elif mutation == "replace":
        replacement = protected / "replacement"
        replacement.write_text("AAAA", encoding="utf-8")
        replacement.replace(file)
    else:
        file.unlink()

    assert file in _find_newer(protected, 0.0, before)


def _uuid(n: int) -> str:
    return f"{n:08x}-0000-4000-8000-{n:012x}"


def test_a_concurrent_agent_session_inbox_write_is_not_contamination(tmp_path: Path) -> None:
    # The real-HOME shape: a protected state root holding the sessions' inbox.
    state = tmp_path / "state" / "shipyard"
    inbox = state / "inbox"
    inbox.mkdir(parents=True)
    old = inbox / f"{_uuid(1)}.jsonl"
    old.write_text("{}\n", encoding="utf-8")
    before = {state: _snapshot_paths(state)}

    # During the run, other agent sessions append, acknowledge and start.
    old.write_text("{}\n{}\n", encoding="utf-8")
    (inbox / f"{_uuid(1)}.shown.jsonl").write_text("{}\n", encoding="utf-8")
    (inbox / f"{_uuid(2)}.jsonl").write_text("{}\n", encoding="utf-8")

    report = contamination((state,), before, 0.0, inbox_dir=inbox)

    assert report.clean, report.format()
    assert len(report.concurrent_sessions) == 3


def test_a_sandbox_write_to_real_home_is_still_flagged(tmp_path: Path) -> None:
    state = tmp_path / "state" / "shipyard"
    inbox = state / "inbox"
    inbox.mkdir(parents=True)
    before = {state: _snapshot_paths(state)}

    queue = state / "queue" / "outcomes" / "sy-1.json"
    queue.parent.mkdir(parents=True)
    queue.write_text("{}", encoding="utf-8")
    named = inbox / "sandbox-session.jsonl"
    named.write_text("{}\n", encoding="utf-8")
    nested = inbox / _uuid(3) / "escape.jsonl"
    nested.parent.mkdir()
    nested.write_text("{}\n", encoding="utf-8")
    beside = state / f"{_uuid(4)}.jsonl"
    beside.write_text("{}\n", encoding="utf-8")
    alongside = inbox / f"{_uuid(5)}.jsonl"
    alongside.write_text("{}\n", encoding="utf-8")

    report = contamination((state,), before, 0.0, inbox_dir=inbox)

    for offender in (queue, named, nested, beside):
        assert offender in report.offenders, report.format()
    assert alongside not in report.offenders
    assert alongside in report.concurrent_sessions
    assert "sandbox wrote outside its isolated HOME/PATH" in report.format()
    assert "not counted (agent sessions' inboxes" in report.format()


def _guard_log_world(tmp_path: Path) -> tuple[Path, Path, Path, dict]:
    """A protected guards root holding a production log with one line."""
    guards = tmp_path / "config" / "shipyard" / "guards"
    guards.mkdir(parents=True)
    log = guards / "holds.jsonl"
    log.write_text('{"pr": 1, "action": "none"}\n', encoding="utf-8")
    other = guards / "queue-arm-guard"
    other.write_text("#!/bin/sh\n", encoding="utf-8")
    return guards, log, other, {guards: _snapshot_paths(guards)}


def test_another_agents_guard_log_append_is_not_contamination(tmp_path: Path) -> None:
    # Shipyard#837: a HOLD another agent posted on the host appended to the
    # real holds.jsonl while the sandbox ran; queue-arms.jsonl is the same.
    guards, log, _, before = _guard_log_world(tmp_path)
    with log.open("a", encoding="utf-8") as handle:
        handle.write('{"pr": 10315, "action": "none", "why": "neither armed nor queued"}\n')
    report = contamination((guards,), before, 0.0, guard_logs=frozenset({log}),
                           sandbox_root=tmp_path / "sandbox")
    assert report.clean, report.format()
    assert log in report.concurrent_sessions
    assert "production guard-log appends" in report.format() or report.clean


def test_a_guard_log_change_that_is_not_a_clean_append_still_counts(tmp_path: Path) -> None:
    sandbox = tmp_path / "sandbox"
    cases = {
        "rewritten": lambda log: log.write_text('{"pr": 2}\n{"pr": 3}\n', encoding="utf-8"),
        "truncated": lambda log: log.write_text("", encoding="utf-8"),
        # Longer than before, so only the unchanged-prefix check catches it.
        "rewritten and grown": lambda log: log.write_text(
            '{"pr": 9, "action": "edit"}\n{"pr": 7, "action": "more"}\n', encoding="utf-8"),
        "not json": lambda log: log.open("a", encoding="utf-8").write("not json\n"),
        "no newline": lambda log: log.open("a", encoding="utf-8").write('{"pr": 4}'),
        "names the sandbox": lambda log: log.open("a", encoding="utf-8").write(
            json.dumps({"cwd": str(sandbox / "work")}) + "\n"),
    }
    for label, change in cases.items():
        world = tmp_path / label.replace(" ", "-")
        guards, log, other, before = _guard_log_world(world)
        change(log)
        report = contamination((guards,), before, 0.0, guard_logs=frozenset({log}),
                               sandbox_root=sandbox)
        assert log in report.offenders, (label, report.format())
    # An append to any other file under the guards directory still counts,
    # and so does a log the list does not name.
    guards, log, other, before = _guard_log_world(tmp_path / "other")
    with other.open("a", encoding="utf-8") as handle:
        handle.write('{"pr": 5}\n')
    with log.open("a", encoding="utf-8") as handle:
        handle.write('{"pr": 6}\n')
    report = contamination((guards,), before, 0.0, guard_logs=frozenset(), sandbox_root=sandbox)
    assert other in report.offenders and log in report.offenders, report.format()


def test_a_row_carrying_the_sandboxs_nonce_is_its_own_leak(tmp_path: Path) -> None:
    # A guard that ran outside the sandbox's HOME writes an ordinary row with
    # no path in it; only the run's identity in that row gives it away.
    nonce = "sandbox-00000000-0000-4000-8000-000000000001"
    guards, log, _, before = _guard_log_world(tmp_path / "leak")
    with log.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps({"at": "2026-10-15T03:00:00Z", "host": "m3", "session": nonce,
                                 "agent": nonce, "pr": 7, "verb": "arm"}) + "\n")
    report = contamination((guards,), before, 0.0, guard_logs=frozenset({log}),
                           sandbox_root=tmp_path / "sandbox", nonce=nonce)
    assert log in report.offenders, report.format()
    # Control: the same row from another session is another agent's append.
    guards, log, _, before = _guard_log_world(tmp_path / "foreign")
    with log.open("a", encoding="utf-8") as handle:
        handle.write(json.dumps({"at": "2026-10-15T03:00:00Z", "host": "m3",
                                 "session": "08fb81d8-38a3-4b20-8bcc-c34ba2a66ffb",
                                 "pr": 7, "verb": "arm"}) + "\n")
    report = contamination((guards,), before, 0.0, guard_logs=frozenset({log}),
                           sandbox_root=tmp_path / "sandbox", nonce=nonce)
    assert report.clean and log in report.concurrent_sessions, report.format()


def test_every_sandbox_process_runs_with_the_runs_nonce_as_its_identity() -> None:
    sandbox = Sandbox()
    sandbox.root = Path("/tmp/shipyard-sandbox-e2e.example")
    try:
        env = sandbox._env(None)
    finally:
        sandbox.root = None
    assert sandbox.nonce.startswith("sandbox-")
    for name in SANDBOX_IDENTITY_ENVS:
        assert env[name] == sandbox.nonce, name
    assert {"SHIPYARD_AGENT_ID", "CLAUDE_CODE_SESSION_ID"} <= set(SANDBOX_IDENTITY_ENVS)
    assert Sandbox().nonce != sandbox.nonce, "a new nonce per run"


def test_the_guard_logs_are_the_real_home_ones() -> None:
    home = Path.home()
    assert home / ".config" / "shipyard" / "guards" / "holds.jsonl" in CONCURRENT_GUARD_LOGS
    assert home / ".local" / "state" / "shipyard" / "queue-arms.jsonl" in CONCURRENT_GUARD_LOGS


def test_the_session_inbox_is_the_real_home_one() -> None:
    assert SESSION_INBOX_DIR == Path.home() / ".local" / "state" / "shipyard" / "inbox"
    assert any(SESSION_INBOX_DIR.is_relative_to(root) for root in PROTECTED_PATHS)
