"""Transient SSH failure retry with exponential backoff.

Detects common transient SSH errors and retries with increasing delays.
Fails fast on permanent errors (auth failures, no route, etc.).
"""

from __future__ import annotations

import functools
import logging
import time
from typing import Any, Callable, TypeVar

logger = logging.getLogger(__name__)

# Patterns that indicate a transient (retryable) SSH failure.
TRANSIENT_PATTERNS: tuple[str, ...] = (
    "Connection reset by peer",
    "kex_exchange_identification",
    "Connection closed by remote host",
    "Connection timed out",
    "ssh_exchange_identification",
    "Connection refused",
    "Network is unreachable",
    "No route to host",
    "broken pipe",
)

T = TypeVar("T")


class SSHTransientError(Exception):
    """Raised when an SSH operation fails with a transient error."""

    def __init__(self, message: str, attempt: int, max_retries: int) -> None:
        self.attempt = attempt
        self.max_retries = max_retries
        super().__init__(message)


class SSHPermanentError(Exception):
    """Raised when an SSH operation fails with a non-retryable error."""


def is_transient(error_message: str) -> bool:
    """Check if an error message matches a known transient SSH pattern."""
    lower = error_message.lower()
    return any(pattern.lower() in lower for pattern in TRANSIENT_PATTERNS)


def retry_ssh(
    func: Callable[..., T] | None = None,
    *,
    max_retries: int = 3,
    backoff_base: float = 2.0,
    _sleep: Callable[[float], Any] = time.sleep,
) -> Any:
    """Retry wrapper for SSH operations with exponential backoff.

    Can be used as a decorator (with or without arguments) or called directly.

    Args:
        func: The function to wrap (when used as bare decorator).
        max_retries: Maximum number of retry attempts.
        backoff_base: Base for exponential backoff (delay = base ** attempt).
        _sleep: Sleep function (injectable for testing).

    Returns:
        Decorated function or decorator.

    Raises:
        SSHTransientError: If all retries are exhausted on transient failures.
        SSHPermanentError: Immediately on non-transient failures.

    Usage::

        @retry_ssh
        def check_host(host: str) -> bool: ...

        @retry_ssh(max_retries=5, backoff_base=1.5)
        def upload_file(path: str) -> None: ...

        # Direct call
        result = retry_ssh(lambda: do_ssh(), max_retries=2)
    """
    if func is None:
        # Called with arguments: @retry_ssh(max_retries=5)
        def decorator(fn: Callable[..., T]) -> Callable[..., T]:
            @functools.wraps(fn)
            def wrapper(*args: Any, **kwargs: Any) -> T:
                return _execute_with_retry(
                    fn, args, kwargs, max_retries, backoff_base, _sleep
                )

            return wrapper

        return decorator

    if callable(func):
        # Called as bare decorator or direct invocation
        @functools.wraps(func)
        def wrapper(*args: Any, **kwargs: Any) -> T:
            return _execute_with_retry(
                func, args, kwargs, max_retries, backoff_base, _sleep
            )

        return wrapper

    raise TypeError(f"retry_ssh expects a callable, got {type(func)}")


def _execute_with_retry(
    func: Callable[..., T],
    args: tuple[Any, ...],
    kwargs: dict[str, Any],
    max_retries: int,
    backoff_base: float,
    sleep_fn: Callable[[float], Any],
) -> T:
    """Execute a function with retry logic."""
    last_error: Exception | None = None

    for attempt in range(max_retries + 1):
        try:
            return func(*args, **kwargs)
        except (SSHTransientError, SSHPermanentError):
            raise
        except Exception as exc:
            error_msg = str(exc)

            if not is_transient(error_msg):
                raise SSHPermanentError(error_msg) from exc

            last_error = exc
            if attempt < max_retries:
                delay = backoff_base ** attempt
                logger.warning(
                    "Transient SSH error (attempt %d/%d), retrying in %.1fs: %s",
                    attempt + 1,
                    max_retries + 1,
                    delay,
                    error_msg,
                )
                sleep_fn(delay)
            else:
                logger.error(
                    "All %d attempts exhausted: %s",
                    max_retries + 1,
                    error_msg,
                )

    raise SSHTransientError(
        str(last_error),
        attempt=max_retries,
        max_retries=max_retries,
    )
