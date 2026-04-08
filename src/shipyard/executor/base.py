"""Abstract executor interface.

Executors are stateless — they receive a job + target config and return
a result. The queue owns state. This makes testing trivial: mock the
executor, test the queue logic independently.
"""

from __future__ import annotations

from typing import Any, Protocol

from shipyard.core.job import TargetResult


class Executor(Protocol):
    """Protocol for validation executors.

    Each backend (local, SSH, cloud) implements this interface.
    """

    def validate(
        self,
        sha: str,
        branch: str,
        target_config: dict[str, Any],
        validation_config: dict[str, Any],
        log_path: str,
    ) -> TargetResult:
        """Run validation on the target and return the result.

        Args:
            sha: The exact commit SHA to validate.
            branch: Branch name (for context/logging).
            target_config: Target definition from config (backend, host, etc.).
            validation_config: Validation commands (build, test, etc.).
            log_path: Where to write the validation log.

        Returns:
            A TargetResult with pass/fail status and metadata.
        """
        ...

    def probe(self, target_config: dict[str, Any]) -> bool:
        """Check whether the target is reachable.

        Returns True if the target can accept validation jobs right now.
        """
        ...
