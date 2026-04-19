"""Runner provider interface.

Providers resolve platform names to concrete runner selectors
(GitHub Actions runner labels, Namespace profiles, etc.). They also
advertise provider profiles — named capability bundles used by the
failover chain's locality-routing filter (see :mod:`shipyard.failover`).
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, Protocol

# Well-known capability vocabulary. Users may define their own strings
# on top of these — unknown capabilities are treated as opaque tags.
KNOWN_CAPABILITIES: frozenset[str] = frozenset(
    {
        "gpu",
        "arm64",
        "x86_64",
        "macos",
        "linux",
        "windows",
        "nested_virt",
        "privileged",
    }
)


@dataclass(frozen=True)
class ProviderProfile:
    """One named profile on a runner provider.

    A profile is what the target's fallback chain is actually filtered
    against: if a target declares ``requires = ["gpu", "x86_64"]``, at
    least one provider profile in the chain must have both capabilities
    listed. Unknown strings pass through unchanged — the matcher is
    pure set containment.
    """

    provider: str
    name: str
    capabilities: frozenset[str] = field(default_factory=frozenset)

    def satisfies(self, requires: list[str] | frozenset[str]) -> bool:
        """Return True iff every required capability is in the profile."""
        need = {str(r).strip() for r in requires if str(r).strip()}
        return need.issubset(self.capabilities)

    @property
    def label(self) -> str:
        """Human-readable ``provider.profile`` label for errors/logs."""
        return f"{self.provider}.{self.name}"


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


def profile_from_config(
    provider: str, profile_name: str, profile_section: dict[str, Any]
) -> ProviderProfile:
    """Build a ProviderProfile from a TOML ``[providers.<p>.profiles.<n>]`` section.

    Tolerant of missing ``capabilities`` — produces an empty-capability
    profile that only satisfies empty ``requires`` lists.
    """
    caps_raw = profile_section.get("capabilities", []) or []
    if not isinstance(caps_raw, list):
        caps_raw = []
    caps = frozenset(str(c).strip() for c in caps_raw if str(c).strip())
    return ProviderProfile(provider=provider, name=profile_name, capabilities=caps)


__all__ = [
    "KNOWN_CAPABILITIES",
    "ProviderProfile",
    "RunnerProvider",
    "profile_from_config",
]
