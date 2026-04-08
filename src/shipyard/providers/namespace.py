"""Namespace runner provider.

Namespace provides ephemeral cloud VMs with profile-based or direct
machine selectors. Resolution chain:

  1. Explicit override in target config
  2. Profile from project config (namespace-profile-<name>)
  3. Repo variable lookup (NAMESPACE_RUNNER_<PLATFORM>)
  4. Error — no silent defaults
"""

from __future__ import annotations

from typing import Any


class NamespaceProvider:
    """Resolves platforms to Namespace runner selectors."""

    def name(self) -> str:
        return "namespace"

    def resolve_selector(self, platform: str, config: dict[str, Any]) -> str:
        """Resolve platform to a Namespace runner selector.

        Resolution chain:
          1. Explicit override in config['runner_overrides'][platform]
          2. Profile name from config['profiles'][platform] -> namespace-profile-<name>
          3. Direct machine label from config['machines'][platform] -> nscloud-<spec>
          4. ValueError

        Args:
            platform: Target platform (e.g. 'linux-x64', 'macos-arm64').
            config: Namespace provider config section.

        Returns:
            A Namespace runner selector string.

        Raises:
            ValueError: If no selector can be resolved.
        """
        # 1. Explicit override
        overrides = config.get("runner_overrides", {})
        if platform in overrides:
            return overrides[platform]

        # 2. Profile-based selector
        profiles = config.get("profiles", {})
        if platform in profiles:
            profile_name = profiles[platform]
            return f"namespace-profile-{profile_name}"

        # 3. Direct machine label
        machines = config.get("machines", {})
        if platform in machines:
            machine_spec = machines[platform]
            if machine_spec.startswith("nscloud-"):
                return machine_spec
            return f"nscloud-{machine_spec}"

        raise ValueError(
            f"No Namespace runner for platform '{platform}'. "
            f"Configure runner_overrides, profiles, or machines in the "
            f"namespace provider config."
        )

    def describe(self, run_metadata: dict[str, Any]) -> str:
        profile = run_metadata.get("runner_profile", "unknown")
        run_id = run_metadata.get("run_id", "?")
        return f"Namespace ({profile}) run #{run_id}"
