"""Tests for SSH retry logic."""

from __future__ import annotations

import pytest

from shipyard.failover.retry import (
    SSHPermanentError,
    SSHTransientError,
    is_transient,
    retry_ssh,
)


class TestIsTransient:
    def test_connection_reset(self) -> None:
        assert is_transient("Connection reset by peer") is True

    def test_kex_exchange(self) -> None:
        assert is_transient("kex_exchange_identification: read: Connection reset") is True

    def test_connection_closed(self) -> None:
        assert is_transient("Connection closed by remote host") is True

    def test_connection_timed_out(self) -> None:
        assert is_transient("Connection timed out") is True

    def test_connection_refused(self) -> None:
        assert is_transient("Connection refused") is True

    def test_case_insensitive(self) -> None:
        assert is_transient("connection RESET by peer") is True

    def test_permanent_error(self) -> None:
        assert is_transient("Permission denied (publickey)") is False

    def test_auth_failure(self) -> None:
        assert is_transient("Authentication failed") is False

    def test_empty_string(self) -> None:
        assert is_transient("") is False


class TestRetrySshDecorator:
    def test_success_no_retry(self) -> None:
        calls = []

        @retry_ssh
        def ok() -> str:
            calls.append(1)
            return "done"

        assert ok() == "done"
        assert len(calls) == 1

    def test_transient_then_success(self) -> None:
        attempts = []
        sleeps: list[float] = []

        @retry_ssh(max_retries=3, _sleep=sleeps.append)
        def flaky() -> str:
            attempts.append(1)
            if len(attempts) < 3:
                raise OSError("Connection reset by peer")
            return "ok"

        assert flaky() == "ok"
        assert len(attempts) == 3
        assert len(sleeps) == 2  # slept between retry 1->2 and 2->3

    def test_permanent_error_fails_fast(self) -> None:
        attempts = []

        @retry_ssh(max_retries=3, _sleep=lambda _: None)
        def bad_auth() -> str:
            attempts.append(1)
            raise OSError("Permission denied (publickey)")

        with pytest.raises(SSHPermanentError, match="Permission denied"):
            bad_auth()

        assert len(attempts) == 1  # no retries

    def test_all_retries_exhausted(self) -> None:
        attempts = []

        @retry_ssh(max_retries=2, _sleep=lambda _: None)
        def always_fails() -> str:
            attempts.append(1)
            raise OSError("Connection timed out")

        with pytest.raises(SSHTransientError):
            always_fails()

        assert len(attempts) == 3  # initial + 2 retries

    def test_exponential_backoff(self) -> None:
        sleeps: list[float] = []

        @retry_ssh(max_retries=3, backoff_base=2.0, _sleep=sleeps.append)
        def fails() -> str:
            raise OSError("Connection reset by peer")

        with pytest.raises(SSHTransientError):
            fails()

        # backoff: 2^0=1, 2^1=2, 2^2=4
        assert sleeps == [1.0, 2.0, 4.0]

    def test_custom_backoff_base(self) -> None:
        sleeps: list[float] = []

        @retry_ssh(max_retries=2, backoff_base=3.0, _sleep=sleeps.append)
        def fails() -> str:
            raise OSError("Connection refused")

        with pytest.raises(SSHTransientError):
            fails()

        # 3^0=1, 3^1=3
        assert sleeps == [1.0, 3.0]

    def test_preserves_return_value(self) -> None:
        @retry_ssh
        def returns_dict() -> dict:
            return {"key": "value"}

        assert returns_dict() == {"key": "value"}

    def test_preserves_function_name(self) -> None:
        @retry_ssh
        def my_function() -> None:
            pass

        assert my_function.__name__ == "my_function"

    def test_decorator_with_args(self) -> None:
        @retry_ssh(max_retries=1)
        def add(a: int, b: int) -> int:
            return a + b

        assert add(2, 3) == 5


class TestSSHTransientError:
    def test_attributes(self) -> None:
        err = SSHTransientError("timeout", attempt=2, max_retries=3)
        assert err.attempt == 2
        assert err.max_retries == 3
        assert "timeout" in str(err)


class TestSSHPermanentError:
    def test_message(self) -> None:
        err = SSHPermanentError("auth failed")
        assert "auth failed" in str(err)
