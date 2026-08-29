import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from shipyard_sandbox import (
    WRITER_DOMAIN_LOCK_NAME,
    WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
    WRITER_DOMAIN_TURNSTILE_NAME,
    WriterDomainLease,
    WriterDomainOverlap,
    _find_newer,
    _snapshot_paths,
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
    guardian_pid = tmp_path / f"{surface}.guardian-pid"
    child = tmp_path / f"{surface}-child.sh"
    child.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        "ready=$1; go=$2; output=$3\n"
        ": > \"$ready\"\n"
        "while [ ! -e \"$go\" ]; do sleep 0.01; done\n"
        "printf guarded > \"$output\"\n",
        encoding="utf-8",
    )
    child.chmod(0o755)
    parent_script = tmp_path / f"{surface}-parent.sh"
    parent_script.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        '"$1" writer-domain-exec --path "$2" -- "$3" "$4" "$5" "$2" &\n'
        "guardian=$!\n"
        'printf "%s" "$guardian" > "$6"\n'
        'wait "$guardian"\n',
        encoding="utf-8",
    )
    parent_script.chmod(0o755)
    env = {**os.environ, "HOME": str(home), "USERPROFILE": str(home)}
    parent = subprocess.Popen(
        [
            str(parent_script),
            str(shipyard_binary),
            str(protected),
            str(child),
            str(ready),
            str(go),
            str(guardian_pid),
        ],
        env=env,
    )
    deadline = time.monotonic() + 5
    while not ready.exists() and time.monotonic() < deadline:
        assert parent.poll() is None
        time.sleep(0.01)
    assert ready.exists()
    guardian = int(guardian_pid.read_text(encoding="utf-8"))

    parent.terminate()
    parent.wait(timeout=5)
    os.kill(guardian, 0)
    audit = WriterDomainLease(production_writer_domain_lock_path(home), exclusive=True)
    with pytest.raises(WriterDomainOverlap):
        audit.acquire(timeout=0.1)
    assert not protected.exists()

    go.touch()
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        try:
            audit.acquire(timeout=0.05)
            break
        except WriterDomainOverlap:
            time.sleep(0.01)
    else:
        pytest.fail("writer-domain guardian did not release after child completion")
    assert protected.read_text(encoding="utf-8") == "guarded"
    audit.release()


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
