"""Abstract tunnel backend interface."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Protocol


class TunnelError(Exception):
    """Base for backend failures."""


class TunnelNotReadyError(TunnelError):
    """Backend isn't installed / configured / authenticated."""


class TunnelStartError(TunnelError):
    """Backend is ready but ``start()`` didn't succeed."""


@dataclass(frozen=True)
class TunnelInfo:
    """Public-facing details of a running tunnel."""

    public_url: str
    backend: str


class TunnelBackend(Protocol):
    """What every tunnel implementation must support."""

    name: str

    async def detect(self) -> bool:
        """Is this backend installed + authenticated + ready to start?"""
        ...

    async def start(self, local_port: int) -> TunnelInfo:
        """Bring the tunnel up. Idempotent: calling twice with the
        same port must not produce a broken config."""
        ...

    async def stop(self) -> None:
        """Tear down. Safe when nothing is configured."""
        ...

    async def verify(self, local_port: int) -> bool:
        """Confirm the tunnel is currently proxying to ``local_port``."""
        ...
