"""Tests for the failover chain logic."""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any
from unittest.mock import MagicMock

import pytest

from shipyard.core.job import TargetResult, TargetStatus
from shipyard.failover.chain import FallbackChain


def _make_result(
    status: TargetStatus,
    backend: str = "mock",
    error_message: str | None = None,
) -> TargetResult:
    return TargetResult(
        target_name="ubuntu",
        platform="linux-x64",
        status=status,
        backend=backend,
        started_at=datetime.now(timezone.utc),
        completed_at=datetime.now(timezone.utc),
        duration_secs=1.0,
        error_message=error_message,
    )


def _mock_executor(
    validate_result: TargetResult,
    probe_ok: bool = True,
) -> MagicMock:
    executor = MagicMock()
    executor.validate.return_value = validate_result
    executor.probe.return_value = probe_ok
    return executor


@pytest.fixture
def target_config() -> dict[str, Any]:
    return {"name": "ubuntu", "platform": "linux-x64"}


@pytest.fixture
def validation_config() -> dict[str, Any]:
    return {"command": "make test"}


class TestFallbackChainPrimary:
    def test_primary_pass_returns_immediately(
        self, target_config: dict, validation_config: dict
    ) -> None:
        result = _make_result(TargetStatus.PASS, backend="ssh")
        ssh_exec = _mock_executor(result)

        chain = FallbackChain(
            backends=[{"type": "ssh", "host": "ubuntu"}],
            executors={"ssh": ssh_exec},
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.PASS
        assert outcome.primary_backend is None  # no failover
        ssh_exec.validate.assert_called_once()

    def test_primary_fail_returns_immediately(
        self, target_config: dict, validation_config: dict
    ) -> None:
        """Test failures are authoritative — no retry on another backend."""
        result = _make_result(TargetStatus.FAIL, backend="ssh")
        ssh_exec = _mock_executor(result)
        cloud_exec = _mock_executor(_make_result(TargetStatus.PASS))

        chain = FallbackChain(
            backends=[
                {"type": "ssh", "host": "ubuntu"},
                {"type": "cloud", "provider": "namespace"},
            ],
            executors={"ssh": ssh_exec, "cloud": cloud_exec},
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.FAIL
        cloud_exec.validate.assert_not_called()


class TestFallbackChainFailover:
    def test_failover_on_error(
        self, target_config: dict, validation_config: dict
    ) -> None:
        ssh_result = _make_result(TargetStatus.ERROR, backend="ssh", error_message="ssh_unreachable")
        cloud_result = _make_result(TargetStatus.PASS, backend="cloud")

        ssh_exec = _mock_executor(ssh_result)
        cloud_exec = _mock_executor(cloud_result)

        chain = FallbackChain(
            backends=[
                {"type": "ssh", "host": "ubuntu"},
                {"type": "cloud", "provider": "namespace"},
            ],
            executors={"ssh": ssh_exec, "cloud": cloud_exec},
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.PASS
        assert outcome.primary_backend == "ssh:ubuntu"
        assert outcome.failover_reason == "ssh_unreachable"
        assert "failover" in outcome.backend

    def test_failover_on_unreachable(
        self, target_config: dict, validation_config: dict
    ) -> None:
        vm_result = _make_result(TargetStatus.UNREACHABLE, backend="vm", error_message="VM not responding")
        cloud_result = _make_result(TargetStatus.PASS, backend="cloud")

        vm_exec = _mock_executor(vm_result)
        cloud_exec = _mock_executor(cloud_result)

        chain = FallbackChain(
            backends=[
                {"type": "vm", "vm_name": "Ubuntu 24.04"},
                {"type": "cloud", "provider": "namespace"},
            ],
            executors={"vm": vm_exec, "cloud": cloud_exec},
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.PASS
        assert outcome.primary_backend == "vm:Ubuntu 24.04"

    def test_probe_failure_skips_backend(
        self, target_config: dict, validation_config: dict
    ) -> None:
        ssh_exec = _mock_executor(_make_result(TargetStatus.PASS), probe_ok=False)
        cloud_result = _make_result(TargetStatus.PASS, backend="cloud")
        cloud_exec = _mock_executor(cloud_result)

        chain = FallbackChain(
            backends=[
                {"type": "ssh", "host": "ubuntu"},
                {"type": "cloud", "provider": "namespace"},
            ],
            executors={"ssh": ssh_exec, "cloud": cloud_exec},
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.PASS
        ssh_exec.validate.assert_not_called()
        cloud_exec.validate.assert_called_once()

    def test_all_backends_exhausted(
        self, target_config: dict, validation_config: dict
    ) -> None:
        ssh_result = _make_result(TargetStatus.ERROR, backend="ssh", error_message="conn refused")
        cloud_result = _make_result(TargetStatus.ERROR, backend="cloud", error_message="quota exceeded")

        chain = FallbackChain(
            backends=[
                {"type": "ssh", "host": "ubuntu"},
                {"type": "cloud", "provider": "namespace"},
            ],
            executors={
                "ssh": _mock_executor(ssh_result),
                "cloud": _mock_executor(cloud_result),
            },
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.ERROR
        assert outcome.primary_backend == "ssh:ubuntu"
        assert "exhausted" in (outcome.failover_reason or "")


class TestFallbackChainEdgeCases:
    def test_empty_backends_returns_error(
        self, target_config: dict, validation_config: dict
    ) -> None:
        chain = FallbackChain(backends=[], executors={})
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.ERROR
        assert "No backends configured" in (outcome.error_message or "")

    def test_unknown_executor_type_skipped(
        self, target_config: dict, validation_config: dict
    ) -> None:
        cloud_result = _make_result(TargetStatus.PASS, backend="cloud")
        cloud_exec = _mock_executor(cloud_result)

        chain = FallbackChain(
            backends=[
                {"type": "magic", "host": "narnia"},
                {"type": "cloud", "provider": "namespace"},
            ],
            executors={"cloud": cloud_exec},
        )
        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")

        assert outcome.status == TargetStatus.PASS

    def test_structured_backend_definitions(
        self, target_config: dict, validation_config: dict
    ) -> None:
        """Verify that backend defs are structured dicts, not string tokens."""
        vm_result = _make_result(TargetStatus.PASS, backend="vm")
        vm_exec = _mock_executor(vm_result)

        backends = [
            {"type": "vm", "vm_name": "Ubuntu 24.04"},
            {"type": "cloud", "provider": "namespace"},
        ]
        chain = FallbackChain(backends=backends, executors={"vm": vm_exec})

        # Verify the backend definitions are dicts
        for b in chain.backends:
            assert isinstance(b, dict)
            assert "type" in b

        outcome = chain.execute("abc", "main", target_config, validation_config, "/tmp/log")
        assert outcome.status == TargetStatus.PASS
