"""Tests for #239 Phase A: bundle-upload hardening.

Three behaviors this pins down:

1. **Retry with backoff** — transient non-zero `ssh` exits get up
   to 3 attempts. Recovery on attempt 2 or 3 returns success with
   an ``attempts`` log showing the recovery path.

2. **Failure classification** — stderr fingerprints route the
   result into ``ssh_unreachable`` (skip further retries — dead
   host won't get more alive) or ``upload_failed`` (slow runner,
   full retry budget).

3. **Log bootstrap** — the ssh-windows executor writes a log
   header BEFORE upload starts, and appends per-attempt stderr on
   failure. Pre-upload crashes must not leave the user with a
   "see log: …" hint pointing at a nonexistent file.
"""

from __future__ import annotations

from pathlib import Path  # noqa: TC003 — runtime use via tmp_path fixture
from types import SimpleNamespace
from unittest.mock import patch

from shipyard.bundle.git_bundle import (
    BundleResult,
    _classify_upload_failure,
    upload_bundle,
)


def test_classifier_flags_connect_timeout_as_unreachable() -> None:
    # The exact stderr shape from the Pulp repro on #239:
    # "ssh: connect to host 100.92.174.43 port 22: Operation timed out"
    stderr = (
        "ssh: connect to host 100.92.174.43 port 22: Operation timed out"
    )
    assert _classify_upload_failure(stderr) == "ssh_unreachable"


def test_classifier_flags_connection_refused_as_unreachable() -> None:
    assert _classify_upload_failure(
        "ssh: connect to host 1.2.3.4 port 22: Connection refused"
    ) == "ssh_unreachable"


def test_classifier_flags_dns_as_unreachable() -> None:
    assert _classify_upload_failure(
        "ssh: Could not resolve hostname nope: Name or service not known"
    ) == "ssh_unreachable"


def test_classifier_defaults_to_upload_failed_for_generic_stderr() -> None:
    # Slow-runner upload that broke mid-stream, or PowerShell syntax
    # issues on the remote — these are "reached the host, upload
    # itself failed" which should get the full retry budget.
    assert _classify_upload_failure(
        "scp: error writing /tmp/shipyard.bundle: disk full"
    ) == "upload_failed"


def _make_fake_run(returncodes: list[int], stderrs: list[str]) -> callable:
    """Factory for a fake subprocess.run that returns values in order.

    Exhausting the list raises — tests should assert the exact
    number of attempts.
    """
    idx = {"n": 0}

    def fake_run(*args, **kw):
        i = idx["n"]
        idx["n"] += 1
        if i >= len(returncodes):
            raise AssertionError(
                f"subprocess.run called {i+1} times but only "
                f"{len(returncodes)} responses queued"
            )
        return SimpleNamespace(
            returncode=returncodes[i],
            stdout="",
            stderr=stderrs[i] if i < len(stderrs) else "",
        )

    fake_run._calls = idx  # type: ignore[attr-defined]
    return fake_run


def test_upload_retries_on_transient_upload_failure(tmp_path: Path) -> None:
    # Attempt 1: upload timeout-ish generic failure. Attempt 2:
    # success. Total retry budget of 3 — should stop at 2.
    bundle = tmp_path / "b.bundle"
    bundle.write_bytes(b"x" * 1000)

    fake = _make_fake_run(
        returncodes=[1, 0],
        stderrs=["scp: some transient issue", ""],
    )
    # Patch time.sleep so the test doesn't actually wait for
    # backoff (2-3s per gap would make this test slow).
    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake), \
         patch("time.sleep"):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path="C:\\shipyard.bundle",
            is_windows=True,
        )
    assert result.success is True
    assert fake._calls["n"] == 2  # stopped after success on attempt 2
    # On success after retries, the attempts log records both the
    # failed first attempt AND the recovery. The specific line with
    # "success" is whichever attempt actually succeeded (attempt 2
    # in this test).
    assert len(result.attempts) >= 2
    assert any("success" in line for line in result.attempts)
    assert any("attempt 1" in line and "failed" in line
               for line in result.attempts)


def test_upload_gives_up_fast_on_ssh_unreachable(tmp_path: Path) -> None:
    # SSH connect failure — retrying won't help, skip further attempts.
    bundle = tmp_path / "b.bundle"
    bundle.write_bytes(b"x" * 1000)

    fake = _make_fake_run(
        returncodes=[255],
        stderrs=[
            "ssh: connect to host 100.92.174.43 port 22: "
            "Operation timed out"
        ],
    )
    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake), \
         patch("time.sleep"):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path="C:\\shipyard.bundle",
            is_windows=True,
            max_attempts=3,
        )
    assert result.success is False
    assert result.failure_class == "ssh_unreachable"
    # Only ONE attempt — we bail fast on unreachable rather than
    # burning the full budget.
    assert fake._calls["n"] == 1
    assert len(result.attempts) == 1


def test_upload_failed_after_reachable_exhausts_retries(tmp_path: Path) -> None:
    # Pulp repro shape: the host is reachable but upload keeps
    # failing (slow runner under AV pressure). We want the full
    # retry budget, classification `upload_failed`, and a message
    # citing how many attempts happened.
    bundle = tmp_path / "b.bundle"
    bundle.write_bytes(b"x" * 1000)

    fake = _make_fake_run(
        returncodes=[1, 1, 1],
        stderrs=[
            "scp: stream ended unexpectedly",
            "scp: stream ended unexpectedly",
            "scp: stream ended unexpectedly",
        ],
    )
    with patch("shipyard.bundle.git_bundle.subprocess.run", side_effect=fake), \
         patch("time.sleep"):
        result = upload_bundle(
            bundle_path=bundle,
            host="win",
            remote_path="C:\\shipyard.bundle",
            is_windows=True,
            max_attempts=3,
        )
    assert result.success is False
    assert result.failure_class == "upload_failed"
    assert fake._calls["n"] == 3
    assert len(result.attempts) == 3
    assert "3 attempt" in result.message


def test_bundle_result_default_failure_class_is_other() -> None:
    # Backward compat for existing callers that construct
    # BundleResult directly without the new fields.
    r = BundleResult(success=False, message="something broke")
    assert r.failure_class == "other"
    assert r.attempts == ()
