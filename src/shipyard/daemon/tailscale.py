"""Tailscale readiness detection.

Mirrors ``TailscaleProbe.swift`` — shells to the Tailscale CLI's
``status --json`` output and decides whether the daemon's Funnel
backend can actually bring up a public tunnel.

Split into pure ``decode()`` (trivial to unit-test) and impure
``probe()`` (runs the subprocess).
"""

from __future__ import annotations

import json
import os
import subprocess
from dataclasses import dataclass
from pathlib import Path

CANDIDATE_BINARIES = (
    "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    "/opt/homebrew/bin/tailscale",
    "/usr/local/bin/tailscale",
    "/usr/bin/tailscale",
)

_FUNNEL_CAP_KEYS = (
    "https://tailscale.com/cap/funnel",
    "funnel",
)


@dataclass(frozen=True)
class TailscaleStatus:
    binary_path: str | None
    backend_state: str | None
    dns_name: str | None
    funnel_permitted: bool

    @property
    def is_ready(self) -> bool:
        return (
            self.binary_path is not None
            and self.backend_state == "Running"
            and bool(self.dns_name)
            and self.funnel_permitted
        )

    @property
    def funnel_url(self) -> str | None:
        if not self.is_ready or not self.dns_name:
            return None
        trimmed = self.dns_name.rstrip(".")
        return f"https://{trimmed}" if trimmed else None


def resolve_binary(
    candidates: tuple[str, ...] = CANDIDATE_BINARIES,
) -> str | None:
    """First Tailscale CLI on disk, or ``None``."""
    for path in candidates:
        if os.access(path, os.X_OK) and Path(path).is_file():
            return path
    return None


def decode(raw_json: bytes, binary_path: str | None) -> TailscaleStatus:
    """Pure parser for ``tailscale status --json`` output."""
    try:
        obj = json.loads(raw_json)
    except (json.JSONDecodeError, UnicodeDecodeError):
        return TailscaleStatus(
            binary_path=binary_path,
            backend_state=None,
            dns_name=None,
            funnel_permitted=False,
        )
    if not isinstance(obj, dict):
        return TailscaleStatus(
            binary_path=binary_path,
            backend_state=None,
            dns_name=None,
            funnel_permitted=False,
        )
    backend_raw = obj.get("BackendState")
    backend = backend_raw if isinstance(backend_raw, str) else None
    self_raw = obj.get("Self")
    self_obj: dict[str, object] = self_raw if isinstance(self_raw, dict) else {}
    dns_raw = self_obj.get("DNSName")
    dns = dns_raw if isinstance(dns_raw, str) else None
    cap_raw = self_obj.get("CapMap")
    cap_map: dict[str, object] = cap_raw if isinstance(cap_raw, dict) else {}
    permitted = any(k in cap_map for k in _FUNNEL_CAP_KEYS)
    return TailscaleStatus(
        binary_path=binary_path,
        backend_state=backend,
        dns_name=dns,
        funnel_permitted=permitted,
    )


def probe(timeout: float = 5.0) -> TailscaleStatus:
    """Run ``tailscale status --json`` and return a parsed snapshot.

    Returns a "not installed" status if the binary isn't on disk, and
    an empty-state status if the subprocess fails.
    """
    binary = resolve_binary()
    if binary is None:
        return TailscaleStatus(
            binary_path=None,
            backend_state=None,
            dns_name=None,
            funnel_permitted=False,
        )
    try:
        result = subprocess.run(
            [binary, "status", "--json"],
            capture_output=True,
            timeout=timeout,
            check=False,
        )
    except (subprocess.TimeoutExpired, OSError):
        return TailscaleStatus(
            binary_path=binary,
            backend_state=None,
            dns_name=None,
            funnel_permitted=False,
        )
    return decode(result.stdout or b"", binary)
