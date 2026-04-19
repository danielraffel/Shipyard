"""Tests for failure classification heuristics."""

from __future__ import annotations

import pytest

from shipyard.core.classify import FailureClass, classify_failure, is_retryable


class TestClassifyFailure:
    def test_contract_violation_takes_priority(self) -> None:
        # Even with infra markers and exit 0, a contract violation is
        # CONTRACT.
        assert (
            classify_failure(
                stdout="",
                stderr="Connection refused by remote host",
                exit_code=0,
                contract_violated=True,
            )
            == FailureClass.CONTRACT
        )

    def test_timeout_takes_priority_over_infra(self) -> None:
        # Infra-looking stderr + timeout → TIMEOUT wins (the timeout
        # is the proximate cause; the infra string is a side effect
        # of the killed process).
        assert (
            classify_failure(
                stdout="",
                stderr="Connection reset by peer",
                exit_code=124,
                wall_clock_exceeded=True,
            )
            == FailureClass.TIMEOUT
        )

    @pytest.mark.parametrize(
        "stderr",
        [
            "ssh: connect to host x failed",
            "Connection refused",
            "Network is unreachable",
            "Could not resolve host: foo.example",
            "RUN_IN_DAYS_DEAD",
            "github runner offline",
            "No route to host",
            "kex_exchange_identification failed",
        ],
    )
    def test_infra_markers_classify_as_infra(self, stderr: str) -> None:
        assert (
            classify_failure(stdout="", stderr=stderr, exit_code=255)
            == FailureClass.INFRA
        )

    def test_non_zero_exit_no_markers_is_test(self) -> None:
        assert (
            classify_failure(
                stdout="AssertionError: expected 2 got 3",
                stderr="",
                exit_code=1,
            )
            == FailureClass.TEST
        )

    def test_zero_exit_no_markers_is_unknown(self) -> None:
        # Caller shouldn't call us here but we must not crash; UNKNOWN
        # is the explicit fallback.
        assert (
            classify_failure(stdout="", stderr="", exit_code=0)
            == FailureClass.UNKNOWN
        )

    def test_infra_marker_embedded_in_larger_stderr(self) -> None:
        stderr = (
            "Running tests...\n"
            "Test foo passed\n"
            "ssh: connect to host bar.local port 22: "
            "Operation timed out\n"
        )
        assert (
            classify_failure(stdout="", stderr=stderr, exit_code=255)
            == FailureClass.INFRA
        )


class TestIsRetryable:
    @pytest.mark.parametrize(
        "cls",
        [FailureClass.INFRA, FailureClass.TIMEOUT],
    )
    def test_infra_and_timeout_are_retryable(self, cls: FailureClass) -> None:
        assert is_retryable(cls) is True

    @pytest.mark.parametrize(
        "cls",
        [FailureClass.CONTRACT, FailureClass.TEST, FailureClass.UNKNOWN],
    )
    def test_authoritative_classes_not_retryable(self, cls: FailureClass) -> None:
        assert is_retryable(cls) is False


class TestShouldRetryFailureClass:
    """The public policy hook used by ship + failover."""

    def test_accepts_enum_string_and_none(self) -> None:
        from shipyard.failover.retry import should_retry_failure_class

        assert should_retry_failure_class(FailureClass.INFRA) is True
        assert should_retry_failure_class("INFRA") is True
        assert should_retry_failure_class("TEST") is False
        assert should_retry_failure_class(None) is False
        # Unknown string → fail safe to False.
        assert should_retry_failure_class("not-a-class") is False
