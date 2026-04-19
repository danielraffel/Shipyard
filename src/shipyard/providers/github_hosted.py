"""GitHub-hosted runner provider.

Maps platform names to standard GitHub Actions runner labels. Ships
default :class:`~shipyard.providers.base.ProviderProfile` entries for
the common GitHub-hosted images so locality-routing works
out-of-the-box without config.
"""

from __future__ import annotations

from typing import Any

from shipyard.providers.base import ProviderProfile, profile_from_config

# Standard GitHub-hosted runner labels by platform
_PLATFORM_MAP: dict[str, str] = {
    "linux-x64": "ubuntu-latest",
    "linux": "ubuntu-latest",
    "ubuntu": "ubuntu-latest",
    "windows-x64": "windows-latest",
    "windows": "windows-latest",
    "macos-arm64": "macos-15",
    "macos-x64": "macos-13",
    "macos": "macos-15",
}


# Built-in profile capabilities for GitHub-hosted runners. Matches the
# images GitHub actually ships today: Linux/Windows runners are x86_64,
# macos-15 is ARM64, macos-13 is x86_64. None of them expose GPUs or
# nested virt by default, so those capabilities stay off.
_DEFAULT_PROFILES: dict[str, frozenset[str]] = {
    "ubuntu-latest": frozenset({"linux", "x86_64"}),
    "windows-latest": frozenset({"windows", "x86_64"}),
    "macos-15": frozenset({"macos", "arm64"}),
    "macos-13": frozenset({"macos", "x86_64"}),
}


class GitHubHostedProvider:
    """Resolves platforms to GitHub-hosted runner labels."""

    def name(self) -> str:
        return "github-hosted"

    def resolve_selector(self, platform: str, config: dict[str, Any]) -> str:
        """Resolve platform to a GitHub-hosted runner label.

        Checks for an explicit override in config first, then falls
        back to the built-in platform map.
        """
        overrides = config.get("runner_overrides", {})
        if platform in overrides:
            return overrides[platform]

        normalized = platform.lower().strip()
        if normalized in _PLATFORM_MAP:
            return _PLATFORM_MAP[normalized]

        raise ValueError(
            f"No GitHub-hosted runner for platform '{platform}'. "
            f"Known platforms: {', '.join(sorted(_PLATFORM_MAP.keys()))}"
        )

    def describe(self, run_metadata: dict[str, Any]) -> str:
        runner = run_metadata.get("runner_label", "unknown")
        run_id = run_metadata.get("run_id", "?")
        return f"GitHub-hosted ({runner}) run #{run_id}"

    def profiles(self, config: dict[str, Any] | None = None) -> dict[str, ProviderProfile]:
        """Return the merged map of built-in + user-defined profiles.

        User-defined profiles under ``[providers.github-hosted.profiles.<name>]``
        override the built-in defaults with the same name.
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
