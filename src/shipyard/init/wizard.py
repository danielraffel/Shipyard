"""Interactive init wizard.

Detects project properties, probes available accounts and hosts,
then generates a .shipyard/config.toml with sensible defaults.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path
from typing import Any

import click

from shipyard.core.config import Config
from shipyard.detect.project import ProjectInfo, detect_project


def _probe_gh_auth() -> str | None:
    """Check if GitHub CLI is authenticated. Returns username or None."""
    try:
        result = subprocess.run(
            ["gh", "auth", "status"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0:
            # Parse username from output
            for line in (result.stdout + result.stderr).splitlines():
                if "Logged in" in line and "as" in line:
                    parts = line.split("as")
                    if len(parts) > 1:
                        return parts[-1].strip().rstrip(")")
            return "authenticated"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def _probe_nsc_auth() -> str | None:
    """Check if Namespace CLI is authenticated. Returns org or None."""
    try:
        result = subprocess.run(
            ["nsc", "auth", "whoami"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        if result.returncode == 0:
            return result.stdout.strip() or "authenticated"
    except (FileNotFoundError, subprocess.TimeoutExpired):
        pass
    return None


def _probe_ssh_host(host: str) -> bool:
    """Check if an SSH host is reachable (quick check)."""
    try:
        result = subprocess.run(
            ["ssh", "-o", "ConnectTimeout=3", "-o", "BatchMode=yes", host, "echo", "ok"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        return result.returncode == 0
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return False


def _ensure_gitignore(path: Path) -> None:
    """Add .shipyard.local/ to .gitignore if not already present."""
    gitignore = path / ".gitignore"
    entry = ".shipyard.local/"

    if gitignore.exists():
        content = gitignore.read_text(encoding="utf-8")
        if entry in content:
            return
        if not content.endswith("\n"):
            content += "\n"
        content += entry + "\n"
        gitignore.write_text(content, encoding="utf-8")
    else:
        gitignore.write_text(entry + "\n", encoding="utf-8")


def _build_config_data(
    info: ProjectInfo,
    project_name: str,
    platforms: list[str],
    cloud_provider: str,
    ssh_hosts: dict[str, str],
) -> dict[str, Any]:
    """Build the config dictionary from gathered information."""
    data: dict[str, Any] = {
        "project": {
            "name": project_name,
            "platforms": platforms,
        },
    }

    # Add ecosystem info
    if info.ecosystems:
        primary = info.ecosystems[0]
        data["project"]["type"] = primary.name

        # Build validation commands from ecosystem
        validation: dict[str, Any] = {"default": {}}
        cmds = primary.commands
        if cmds.install:
            validation["default"]["install"] = cmds.install
        if cmds.build:
            validation["default"]["build"] = cmds.build
        if cmds.test:
            validation["default"]["test"] = cmds.test
        if cmds.validate:
            validation["default"]["validate"] = cmds.validate
        if validation["default"]:
            data["validation"] = validation

    # Add targets
    targets: dict[str, Any] = {}
    if "macos" in platforms:
        targets["mac"] = {
            "backend": "local",
            "platform": "macos-arm64",
        }
    if "linux" in platforms:
        linux_target: dict[str, Any] = {"platform": "linux-x64"}
        if "ubuntu" in ssh_hosts or "linux" in ssh_hosts:
            host = ssh_hosts.get("ubuntu") or ssh_hosts.get("linux", "")
            linux_target["backend"] = "ssh"
            linux_target["host"] = host
        else:
            linux_target["backend"] = "cloud"
        targets["ubuntu"] = linux_target
    if "windows" in platforms:
        win_target: dict[str, Any] = {"platform": "windows-x64"}
        if "windows" in ssh_hosts or "win" in ssh_hosts:
            host = ssh_hosts.get("windows") or ssh_hosts.get("win", "")
            win_target["backend"] = "ssh"
            win_target["host"] = host
        else:
            win_target["backend"] = "cloud"
        targets["windows"] = win_target

    if targets:
        data["targets"] = targets

    # Cloud provider
    if cloud_provider != "github-hosted":
        data["cloud"] = {"provider": cloud_provider}

    return data


def run_init(
    path: Path | str | None = None,
    non_interactive: bool = False,
) -> Config:
    """Run the init wizard.

    Detects project ecosystems, probes accounts, and generates
    .shipyard/config.toml. In interactive mode, prompts the user
    for confirmation and overrides.

    Returns the generated Config.
    """
    path = Path(path) if path else Path.cwd()

    # Step 1: Detect project
    info = detect_project(path)

    # Step 2: Derive defaults
    project_name = path.name
    platforms = info.platforms or ["macos", "linux"]
    cloud_provider = "github-hosted"
    ssh_hosts: dict[str, str] = {}

    if not non_interactive:
        click.echo()
        click.echo("Shipyard init")
        click.echo("=" * 40)

        # Show detection results
        if info.ecosystems:
            names = ", ".join(e.name for e in info.ecosystems)
            click.echo(f"  Detected: {names}")
        else:
            click.echo("  No ecosystem detected")

        if info.existing_ci:
            ci_names = ", ".join(c.name for c in info.existing_ci)
            click.echo(f"  Existing CI: {ci_names}")

        if info.git_remote:
            click.echo(f"  Git remote: {info.git_remote}")

        click.echo()

        # Confirm project name
        project_name = click.prompt("Project name", default=project_name)

        # Confirm platforms
        platform_str = click.prompt(
            "Platforms (comma-separated)",
            default=",".join(platforms),
        )
        platforms = [p.strip() for p in platform_str.split(",") if p.strip()]

        # Probe accounts
        click.echo()
        click.echo("Probing accounts...")
        gh_user = _probe_gh_auth()
        nsc_org = _probe_nsc_auth()

        if gh_user:
            click.echo(f"  GitHub: {gh_user}")
        else:
            click.echo("  GitHub: not authenticated (run 'gh auth login')")

        if nsc_org:
            click.echo(f"  Namespace: {nsc_org}")
            cloud_provider = "namespace"
        else:
            click.echo("  Namespace: not configured")

        # Ask about cloud provider
        cloud_provider = click.prompt(
            "Cloud provider",
            default=cloud_provider,
            type=click.Choice(["github-hosted", "namespace"], case_sensitive=False),
        )

        # Probe SSH hosts
        click.echo()
        if click.confirm("Configure SSH targets?", default=False):
            for platform in ["ubuntu", "windows"]:
                if platform == "ubuntu" and "linux" in platforms:
                    host = click.prompt(f"  SSH host for Linux", default="ubuntu")
                    if _probe_ssh_host(host):
                        click.echo(f"    {host}: reachable")
                        ssh_hosts["ubuntu"] = host
                    else:
                        click.echo(f"    {host}: not reachable (skipping)")
                elif platform == "windows" and "windows" in platforms:
                    host = click.prompt(f"  SSH host for Windows", default="win")
                    if _probe_ssh_host(host):
                        click.echo(f"    {host}: reachable")
                        ssh_hosts["windows"] = host
                    else:
                        click.echo(f"    {host}: not reachable (skipping)")

    # Step 3: Build config
    config_data = _build_config_data(info, project_name, platforms, cloud_provider, ssh_hosts)

    # Step 4: Write config
    shipyard_dir = path / ".shipyard"
    shipyard_dir.mkdir(parents=True, exist_ok=True)

    config = Config(data=config_data, project_dir=shipyard_dir)
    config.save_project()

    # Step 5: Update .gitignore
    _ensure_gitignore(path)

    if not non_interactive:
        click.echo()
        click.echo(f"Created .shipyard/config.toml")
        click.echo("Run 'shipyard run' to validate.")

    return config
