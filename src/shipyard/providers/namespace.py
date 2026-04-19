"""Namespace runner provider.

Namespace provides ephemeral cloud VMs with profile-based or direct
machine selectors. Resolution chain:

  1. Explicit override in target config
  2. Profile from project config (namespace-profile-<name>)
  3. Repo variable lookup (NAMESPACE_RUNNER_<PLATFORM>)
  4. Error — no silent defaults

The ``[providers.namespace.profiles]`` section accepts two shapes,
disambiguated by value type:

* ``platform = "profile-name"`` (string value) — legacy platform →
  profile-name mapping used by :meth:`resolve_selector`.
* ``profile-name = { capabilities = [...] }`` (table value) — locality
  routing :class:`~shipyard.providers.base.ProviderProfile` definition
  consumed by :mod:`shipyard.failover.chain`.

Built-in capability profiles ship for ``default`` and ``gpu`` so the
common cases work without any extra config.
"""

from __future__ import annotations

from typing import Any

from shipyard.providers.base import ProviderProfile, profile_from_config

# Built-in capability profiles for Namespace. `default` covers the
# bulk of Namespace's current fleet (multi-arch Linux, nested virt
# for macOS/Windows VMs); `gpu` is the GPU-enabled lane. Users can
# override these by defining a same-named profile in config.
_DEFAULT_PROFILES: dict[str, frozenset[str]] = {
    "default": frozenset(
        {"x86_64", "arm64", "linux", "macos", "windows", "nested_virt"}
    ),
    "gpu": frozenset({"gpu", "x86_64", "linux"}),
}


class NamespaceProvider:
    """Resolves platforms to Namespace runner selectors."""

    def name(self) -> str:
        return "namespace"

    def resolve_selector(self, platform: str, config: dict[str, Any]) -> str:
        """Resolve platform to a Namespace runner selector.

        Resolution chain:
          1. Explicit override in config['runner_overrides'][platform]
          2. Profile name from config['profiles'][platform] -> namespace-profile-<name>
             (legacy string-valued entry; table-valued entries are
             capability-profile definitions, not platform mappings)
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

        # 2. Profile-based selector — only honor string values; table
        #    values are capability-profile definitions handled by
        #    :meth:`profiles`.
        profiles = config.get("profiles", {})
        if platform in profiles and isinstance(profiles[platform], str):
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

    def profiles(self, config: dict[str, Any] | None = None) -> dict[str, ProviderProfile]:
        """Return the merged map of built-in + user-defined capability profiles.

        Only table-valued entries under ``[providers.namespace.profiles.<name>]``
        count as capability profiles; string entries (``platform =
        "profile-name"``) are the legacy platform→profile-name map
        used by :meth:`resolve_selector` and are ignored here.
        """
        merged: dict[str, ProviderProfile] = {
            pname: ProviderProfile(
                provider=self.name(), name=pname, capabilities=caps
            )
            for pname, caps in _DEFAULT_PROFILES.items()
        }
        user_profiles = (config or {}).get("profiles", {}) or {}
        if isinstance(user_profiles, dict):
            for pname, section in user_profiles.items():
                if isinstance(section, dict):
                    merged[pname] = profile_from_config(self.name(), pname, section)
        return merged
