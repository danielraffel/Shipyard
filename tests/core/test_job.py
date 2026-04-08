"""Tests for Job and TargetResult types."""

from __future__ import annotations

import pytest

from shipyard.core.job import (
    Job,
    JobStatus,
    Priority,
    TargetResult,
    TargetStatus,
    ValidationMode,
)


class TestJob:
    def test_create_generates_id(self) -> None:
        job = Job.create(sha="abc123", branch="main", target_names=["mac"])
        assert job.id.startswith("sy-")
        assert job.status == JobStatus.PENDING
        assert job.sha == "abc123"
        assert job.branch == "main"
        assert job.target_names == ("mac",)

    def test_create_defaults(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        assert job.priority == Priority.NORMAL
        assert job.mode == ValidationMode.FULL
        assert job.started_at is None
        assert job.completed_at is None
        assert job.results == {}

    def test_start_transition(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        started = job.start()
        assert started.status == JobStatus.RUNNING
        assert started.started_at is not None
        assert started.id == job.id  # same job

    def test_start_from_running_raises(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start()
        with pytest.raises(ValueError, match="Cannot start"):
            job.start()

    def test_complete_transition(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start()
        completed = job.complete()
        assert completed.status == JobStatus.COMPLETED
        assert completed.completed_at is not None

    def test_complete_from_pending_raises(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        with pytest.raises(ValueError, match="Cannot complete"):
            job.complete()

    def test_cancel_from_pending(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        cancelled = job.cancel()
        assert cancelled.status == JobStatus.CANCELLED

    def test_cancel_from_running(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start()
        cancelled = job.cancel()
        assert cancelled.status == JobStatus.CANCELLED

    def test_cancel_from_completed_raises(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start().complete()
        with pytest.raises(ValueError, match="Cannot cancel"):
            job.cancel()

    def test_with_result_adds_result(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start()
        result = TargetResult(
            target_name="mac",
            platform="macos-arm64",
            status=TargetStatus.PASS,
            backend="local",
            duration_secs=120.5,
        )
        updated = job.with_result(result)
        assert "mac" in updated.results
        assert updated.results["mac"].passed

    def test_with_result_is_immutable(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start()
        result = TargetResult(
            target_name="mac",
            platform="macos-arm64",
            status=TargetStatus.PASS,
            backend="local",
        )
        updated = job.with_result(result)
        assert "mac" not in job.results  # original unchanged
        assert "mac" in updated.results

    def test_passed_when_all_green(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac", "ubuntu"]).start()
        job = job.with_result(TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.PASS, backend="local",
        ))
        job = job.with_result(TargetResult(
            target_name="ubuntu", platform="linux-x64",
            status=TargetStatus.PASS, backend="ssh",
        ))
        job = job.complete()
        assert job.passed

    def test_not_passed_when_one_fails(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac", "ubuntu"]).start()
        job = job.with_result(TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.PASS, backend="local",
        ))
        job = job.with_result(TargetResult(
            target_name="ubuntu", platform="linux-x64",
            status=TargetStatus.FAIL, backend="ssh",
        ))
        job = job.complete()
        assert not job.passed

    def test_all_targets_terminal(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"]).start()
        assert not job.all_targets_terminal
        job = job.with_result(TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.PASS, backend="local",
        ))
        assert job.all_targets_terminal

    def test_to_dict_roundtrip(self) -> None:
        job = Job.create(sha="abc", branch="main", target_names=["mac"])
        d = job.to_dict()
        assert d["sha"] == "abc"
        assert d["branch"] == "main"
        assert d["status"] == "pending"
        assert d["mode"] == "full"
        assert d["targets"] == ["mac"]


class TestTargetResult:
    def test_pass_status(self) -> None:
        r = TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.PASS, backend="local",
        )
        assert r.passed
        assert r.is_terminal

    def test_fail_status(self) -> None:
        r = TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.FAIL, backend="local",
        )
        assert not r.passed
        assert r.is_terminal

    def test_running_not_terminal(self) -> None:
        r = TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.RUNNING, backend="local",
        )
        assert not r.is_terminal

    def test_to_dict_minimal(self) -> None:
        r = TargetResult(
            target_name="mac", platform="macos-arm64",
            status=TargetStatus.PASS, backend="local",
        )
        d = r.to_dict()
        assert d["target"] == "mac"
        assert d["platform"] == "macos-arm64"
        assert d["status"] == "pass"
        assert d["backend"] == "local"
        assert "duration_secs" not in d  # omitted when None

    def test_to_dict_with_failover(self) -> None:
        r = TargetResult(
            target_name="ubuntu", platform="linux-x64",
            status=TargetStatus.PASS, backend="namespace-failover",
            primary_backend="ssh", failover_reason="ssh_unreachable",
            provider="namespace", runner_profile="namespace-profile-default",
        )
        d = r.to_dict()
        assert d["primary_backend"] == "ssh"
        assert d["failover_reason"] == "ssh_unreachable"
        assert d["provider"] == "namespace"
