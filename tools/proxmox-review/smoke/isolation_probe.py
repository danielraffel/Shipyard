#!/usr/bin/env python3
"""One named negative assertion executed inside a disposable review guest."""

from __future__ import annotations

import os
from pathlib import Path
import socket
import sys


def assert_unreachable(host: str, port: int) -> None:
    try:
        with socket.create_connection((host, port), timeout=1):
            raise AssertionError(f"unexpected network access to {host}:{port}")
    except (OSError, TimeoutError):
        pass


probe = sys.argv[1]
if probe == "identity":
    assert os.getuid() != 0
    assert set(os.getgroups()) <= {os.getgid()}
elif probe == "mac-home":
    assert not Path("/Users/danielraffel").exists()
elif probe == "control-secret":
    assert not Path("/var/lib/shipyard-review/secrets/proxmox-api-token").exists()
elif probe == "root-ssh":
    authorized_keys = Path("/root/.ssh/authorized_keys")
    assert not os.access(authorized_keys, os.R_OK)
elif probe == "nested-virtualization":
    assert not Path("/dev/kvm").exists()
    assert "vmx" not in Path("/proc/cpuinfo").read_text(encoding="utf-8", errors="replace")
elif probe == "root-filesystem":
    try:
        Path("/shipyard-host-escape-probe").write_text("blocked", encoding="utf-8")
    except PermissionError:
        pass
    else:
        raise AssertionError("guest user wrote to the root filesystem")
elif probe == "proxmox-bridge":
    assert_unreachable("10.77.0.1", 8006)
elif probe == "proxmox-management":
    assert_unreachable("192.168.86.70", 8006)
elif probe == "internet":
    assert_unreachable("1.1.1.1", 443)
else:
    raise AssertionError(f"unknown protected probe: {probe}")

print(f"isolation probe passed: {probe}")
