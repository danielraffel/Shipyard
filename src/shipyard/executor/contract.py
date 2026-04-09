"""Validation contract evaluation.

A "validation contract" is a project-declared expectation that the
validation script will emit specific marker strings during the run.
For example, Pulp's `validate-build.sh` emits `__PULP_VALIDATION__:smoke`
when running smoke validation and `__PULP_VALIDATION__:full` when running
full validation. If neither marker appears in the output, the run is
not authentic — either the script ran the wrong code path or it was
bypassed entirely — and the result should be treated as a failure
regardless of the process exit code.

The contract is configured per project in `.shipyard/config.toml`:

    [validation.contract]
    markers = ["__PULP_VALIDATION__:smoke", "__PULP_VALIDATION__:full"]
    require_at_least_one = true        # default: true
    enforce = true                     # default: true (failing missing → FAIL)
                                       #          false → record as warning only

Three modes:

- `enforce = true`, `require_at_least_one = true`:
    At least one marker must appear. Missing → FAIL.
- `enforce = true`, `require_at_least_one = false`:
    All declared markers must appear. Any missing → FAIL.
- `enforce = false`:
    Markers are recorded for visibility but never fail the run.

The streaming layer records which markers appeared. This module
evaluates the result against the contract and returns the (possibly
modified) status + a violation message.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any


@dataclass(frozen=True)
class ContractEvaluation:
    """The outcome of evaluating a contract against a streamed result."""

    seen: tuple[str, ...]
    missing: tuple[str, ...]
    violated: bool      # True if the contract requires action and was violated
    enforce: bool       # Whether enforcement is on (drives FAIL vs WARN)
    message: str | None  # Human-readable explanation when violated

    @property
    def should_force_fail(self) -> bool:
        """Whether this evaluation should override a passing exit code."""
        return self.violated and self.enforce


def evaluate_contract(
    contract_config: dict[str, Any] | None,
    seen_markers: tuple[str, ...],
) -> ContractEvaluation:
    """Compare seen markers against the project's contract config.

    `contract_config` is the parsed `[validation.contract]` table from
    `.shipyard/config.toml`, or None if no contract is declared. When
    None, the evaluation is a no-op (no markers required, no violation).

    `seen_markers` is the tuple recorded by `run_streaming_command` —
    every required marker that appeared at least once in the output.
    """
    if not contract_config:
        return ContractEvaluation(
            seen=seen_markers,
            missing=(),
            violated=False,
            enforce=False,
            message=None,
        )

    declared_markers = tuple(contract_config.get("markers", ()))
    if not declared_markers:
        return ContractEvaluation(
            seen=seen_markers,
            missing=(),
            violated=False,
            enforce=False,
            message=None,
        )

    require_at_least_one = bool(contract_config.get("require_at_least_one", True))
    enforce = bool(contract_config.get("enforce", True))

    seen_set = set(seen_markers)
    missing = tuple(m for m in declared_markers if m not in seen_set)

    if require_at_least_one:
        violated = len(seen_set & set(declared_markers)) == 0
        if violated:
            message = (
                f"Validation contract requires at least one of "
                f"{list(declared_markers)} to appear in the output. "
                f"None were observed."
            )
        else:
            message = None
            missing = ()  # at-least-one mode does not flag absent markers
    else:
        violated = len(missing) > 0
        if violated:
            message = (
                f"Validation contract requires every declared marker to "
                f"appear in the output. Missing: {list(missing)}."
            )
        else:
            message = None

    return ContractEvaluation(
        seen=seen_markers,
        missing=missing,
        violated=violated,
        enforce=enforce,
        message=message,
    )


def required_markers(contract_config: dict[str, Any] | None) -> tuple[str, ...]:
    """Extract the marker list to pass to `run_streaming_command`."""
    if not contract_config:
        return ()
    return tuple(contract_config.get("markers", ()))
