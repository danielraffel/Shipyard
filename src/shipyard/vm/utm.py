"""UTM adapter — manage VMs via the utmctl command-line tool.

UTM is a macOS virtualization app. The utmctl CLI provides
list/start/stop operations. This adapter maps VM names to SSH
hosts for remote validation.
"""

from __future__ import annotations

import subprocess
import time
from typing import Any

from shipyard.vm.base import VM


class UTMProvider:
    """Manage VMs via utmctl."""

    def __init__(self, ssh_map: dict[str, str] | None = None) -> None:
        """Initialize with an optional VM name -> SSH host mapping.

        Args:
            ssh_map: Maps VM display names to SSH host/alias strings.
                     Example: {"Ubuntu 24.04": "ubuntu", "Windows 11": "win"}
        """
        self.ssh_map = ssh_map or {}

    def detect(self) -> list[VM]:
        """List all VMs known to UTM."""
        try:
            result = subprocess.run(
                ["utmctl", "list"],
                capture_output=True,
                text=True,
                timeout=10,
            )
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return []

        if result.returncode != 0:
            return []

        vms: list[VM] = []
        for line in result.stdout.strip().splitlines():
            # utmctl list output format: UUID  Name  Status
            parts = line.split(None, 2)
            if len(parts) >= 3:
                uuid, status, name = parts[0], parts[1], parts[2]
                # Some versions: UUID  Status  Name
                # Normalize: if second field looks like a UUID, swap
                vms.append(VM(name=name.strip(), status=status.lower(), uuid=uuid))

        return vms

    def start(self, vm_name: str) -> bool:
        """Start a UTM VM by name."""
        try:
            result = subprocess.run(
                ["utmctl", "start", vm_name],
                capture_output=True,
                timeout=30,
            )
            return result.returncode == 0
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return False

    def stop(self, vm_name: str) -> bool:
        """Stop a UTM VM by name."""
        try:
            result = subprocess.run(
                ["utmctl", "stop", vm_name],
                capture_output=True,
                timeout=30,
            )
            return result.returncode == 0
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return False

    def is_running(self, vm_name: str) -> bool:
        """Check if a VM is running by querying its status."""
        for vm in self.detect():
            if vm.name == vm_name:
                return vm.status == "running"
        return False

    def ssh_host_for(self, vm_name: str) -> str | None:
        """Get the SSH host/alias for a VM name."""
        return self.ssh_map.get(vm_name)

    def boot_and_wait(
        self,
        vm_name: str,
        ssh_host: str,
        timeout_secs: float = 60,
    ) -> bool:
        """Boot a VM and wait until SSH is reachable.

        Args:
            vm_name: UTM VM display name.
            ssh_host: SSH host/alias to probe.
            timeout_secs: Maximum seconds to wait for SSH.

        Returns:
            True if the VM booted and SSH became reachable within the timeout.
        """
        # Start the VM if not already running
        if not self.is_running(vm_name):
            if not self.start(vm_name):
                return False

        # Poll SSH until reachable or timeout
        deadline = time.monotonic() + timeout_secs
        while time.monotonic() < deadline:
            if self._ssh_reachable(ssh_host):
                return True
            time.sleep(2)

        return False

    def _ssh_reachable(self, ssh_host: str) -> bool:
        """Check if an SSH host accepts connections."""
        try:
            result = subprocess.run(
                [
                    "ssh", "-o", "ConnectTimeout=3",
                    "-o", "StrictHostKeyChecking=no",
                    "-o", "BatchMode=yes",
                    ssh_host, "echo", "ok",
                ],
                capture_output=True,
                text=True,
                timeout=10,
            )
            return result.returncode == 0 and "ok" in result.stdout
        except (FileNotFoundError, subprocess.TimeoutExpired):
            return False
