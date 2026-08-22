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


def test_protected_write_is_never_allowlisted_by_filename(tmp_path: Path) -> None:
    sentinel_mtime = 1_000.0
    queue_temp = tmp_path / ".queue-85829-1785087066142925000-0.json.tmp"
    outcome = tmp_path / "queue" / "outcomes" / "sy-existing.json"
    outcome.parent.mkdir(parents=True)
    queue_temp.write_text("temp", encoding="utf-8")
    outcome.write_text("{}", encoding="utf-8")

    offenders = _find_newer(tmp_path, sentinel_mtime, set())

    assert queue_temp in offenders
    assert outcome in offenders
