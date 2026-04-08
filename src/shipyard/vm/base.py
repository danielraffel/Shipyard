"""VM provider interface.

VM providers detect, start, and stop virtual machines for use as
validation targets. Each provider adapts a specific hypervisor
(UTM, Parallels, Tart, etc.).
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol


@dataclass(frozen=True)
class VM:
    """A detected virtual machine."""

    name: str
    status: str  # "running", "stopped", "suspended", etc.
    uuid: str | None = None


class VMProvider(Protocol):
    """Protocol for VM hypervisor adapters."""

    def detect(self) -> list[VM]:
        """List all available VMs.

        Returns:
            List of VM instances with current status.
        """
        ...

    def start(self, vm_name: str) -> bool:
        """Start a VM by name.

        Returns:
            True if the VM was started (or was already running).
        """
        ...

    def stop(self, vm_name: str) -> bool:
        """Stop a VM by name.

        Returns:
            True if the VM was stopped (or was already stopped).
        """
        ...

    def is_running(self, vm_name: str) -> bool:
        """Check whether a VM is currently running."""
        ...
