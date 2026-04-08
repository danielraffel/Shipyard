"""GitHub-hosted runner provider.

Maps platform names to standard GitHub Actions runner labels.
"""

from __future__ import annotations

from typing import Any

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
