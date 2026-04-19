"""Failure classification.

Parse a target's failure into a coarse-grained class so agents and the
ship layer can make retry / merge / escalation decisions without
reading raw logs. Classification is a pure function — heuristics over
the combined stdout / stderr, exit code, and two explicit booleans that
the executor already knows:

- ``wall_clock_exceeded`` — the executor hit its timeout budget
- ``contract_violated`` — the validation contract (``[validation.contract]``)
  was not satisfied

The five classes:

- ``CONTRACT`` — validation contract was not satisfied (wrong code path,
  script bypassed). Never retryable.
- ``TIMEOUT`` — process hit the wall-clock cap. Retryable once.
- ``INFRA`` — network / SSH / runner availability problem. Retryable
  once on the next backend in the chain.
- ``TEST`` — non-zero exit, no infra markers, no contract violation.
  Authoritative test failure; never retry.
- ``UNKNOWN`` — fallback. Treated as ``TEST`` by the ship layer (don't
  retry, don't merge) but surfaced separately so agents can flag it.

Ordering matters: ``contract_violated`` and ``wall_clock_exceeded`` are
checked first because an infra-error fingerprint in stderr plus a
timeout should still be classified as ``TIMEOUT`` (the timeout is the
proximate cause — infra markers may appear in the tail of output
because the executor killed the process).
"""

from __future__ import annotations

from enum import Enum


class FailureClass(str, Enum):
    """Coarse-grained failure taxonomy.

    Values are plain strings so the enum serializes cleanly in JSON.
    """

    INFRA = "INFRA"
    TIMEOUT = "TIMEOUT"
    CONTRACT = "CONTRACT"
    TEST = "TEST"
    UNKNOWN = "UNKNOWN"


# Substrings whose presence in stderr indicates an infrastructure
# problem rather than a test or product failure. Kept explicit (not
# regex) so the list stays reviewable and agents can extend it via
# project config without writing regex.
_INFRA_MARKERS: tuple[str, ...] = (
    "Connection refused",
    "ssh: connect",
    "Network is unreachable",
    "Could not resolve host",
    "RUN_IN_DAYS_DEAD",
    "github runner offline",
    # Common secondary fingerprints we've seen in production.
    "No route to host",
    "kex_exchange_identification",
    "Connection reset by peer",
    "Connection closed by remote host",
    "Connection timed out",
    "ssh_exchange_identification",
)


def classify_failure(
    stdout: str,
    stderr: str,
    exit_code: int,
    wall_clock_exceeded: bool = False,
    contract_violated: bool = False,
) -> FailureClass:
    """Classify a failure into one of the five ``FailureClass`` values.

    Only call this for non-success outcomes. A zero exit code with no
    contract violation and no timeout is a PASS — the caller should
    not be classifying it.

    Heuristic precedence (first match wins):

    1. ``contract_violated`` → ``CONTRACT``
    2. ``wall_clock_exceeded`` → ``TIMEOUT``
    3. Any ``_INFRA_MARKERS`` substring in stderr → ``INFRA``
    4. Non-zero exit with no markers → ``TEST``
    5. Fallback → ``UNKNOWN``
    """
    if contract_violated:
        return FailureClass.CONTRACT

    if wall_clock_exceeded:
        return FailureClass.TIMEOUT

    stderr_blob = stderr or ""
    for marker in _INFRA_MARKERS:
        if marker in stderr_blob:
            return FailureClass.INFRA

    if exit_code != 0:
        return FailureClass.TEST

    return FailureClass.UNKNOWN


def is_retryable(failure_class: FailureClass) -> bool:
    """Whether this failure class is worth retrying once.

    Auto-retry lane, called by ``failover/retry.py`` and the ship layer:

    - ``INFRA`` / ``TIMEOUT`` → retry (transient by nature)
    - ``CONTRACT`` / ``TEST`` → no retry (authoritative)
    - ``UNKNOWN`` → no retry (fail safe; surface to the agent)
    """
    return failure_class in (FailureClass.INFRA, FailureClass.TIMEOUT)
