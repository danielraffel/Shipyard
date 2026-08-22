import json
import os
import subprocess
from pathlib import Path

import pytest

from shipyard_sandbox import (
    WRITER_DOMAIN_LOCK_NAME,
    WRITER_DOMAIN_OVERLAP_CLASSIFICATION,
    WriterDomainLease,
    WriterDomainOverlap,
    _find_newer,
    production_writer_domain_lock_path,
)


def test_production_writer_domain_lock_is_under_machine_state() -> None:
    home = Path("/host-home")
    path = production_writer_domain_lock_path(home)

    assert path.name == WRITER_DOMAIN_LOCK_NAME
    assert "shipyard" in path.parts
    assert path.is_relative_to(home)


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
        assert "production writer" in str(captured.value)
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


def test_production_binary_fails_with_stable_overlap_classification(
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
            # The Rust boundary waits one second; leave launch/sanitizer
            # overhead outside that contract without permitting an unbounded
            # child hang.
            timeout=5,
        )
    finally:
        audit.release()

    assert result.returncode == 75
    assert result.stderr.startswith(WRITER_DOMAIN_OVERLAP_CLASSIFICATION)
    assert result.stdout == ""


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
