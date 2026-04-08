"""Runner provider interface.

Providers resolve platform names to concrete runner selectors
(GitHub Actions runner labels, Namespace profiles, etc.).
"""

from __future__ import annotations

from typing import Any, Protocol


class RunnerProvider(Protocol):
    """Protocol for cloud runner providers.

    Each provider (GitHub-hosted, Namespace, custom) maps platform
    names to runner selector strings used in GitHub Actions workflows.
    """

    def name(self) -> str:
        """Short identifier for this provider (e.g. 'github-hosted', 'namespace')."""
        ...

    def resolve_selector(self, platform: str, config: dict[str, Any]) -> str:
        """Resolve a platform name to a runner selector string.

        Args:
            platform: Target platform (e.g. 'linux-x64', 'macos-arm64').
            config: Provider-specific configuration from the project config.

        Returns:
            A runner label string usable in GitHub Actions 'runs-on'.

        Raises:
            ValueError: If the platform cannot be resolved.
        """
        ...

    def describe(self, run_metadata: dict[str, Any]) -> str:
        """Human-readable description of a run for logging/output.

        Args:
            run_metadata: Metadata from a completed run (provider-specific).

        Returns:
            A short description string.
        """
        ...
