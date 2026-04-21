"""Secret storage for the webhook HMAC.

macOS: the user's keychain via the ``security`` CLI. A single generic-
password entry keyed by service + account.

Linux: a plain file at ``<state_dir>/daemon/webhook-secret`` with 600
permissions. Not as strong as the keychain, but well within the
threat model for a solo-dev daemon; tightening (e.g. libsecret via
``secret-tool``) is deferred.

Windows isn't shipped in v1. Callers should prefer
``load_or_create()`` which handles the "no pre-existing secret" path
by generating a fresh one.
"""

from __future__ import annotations

import contextlib
import os
import subprocess
import sys
from typing import TYPE_CHECKING

from shipyard.daemon import signature

if TYPE_CHECKING:
    from pathlib import Path

SERVICE = "com.danielraffel.shipyard.webhook"
ACCOUNT = "shared"


def load_or_create(state_dir: Path) -> str:
    """Return the stored secret, generating + persisting one on first
    call. Idempotent."""
    existing = load(state_dir)
    if existing is not None:
        return existing
    fresh = signature.generate_secret()
    save(fresh, state_dir)
    return fresh


def load(state_dir: Path) -> str | None:
    if sys.platform == "darwin":
        value = _keychain_find()
        if value is not None:
            return value
    path = _file_path(state_dir)
    if path.exists():
        return path.read_text(encoding="utf-8").strip() or None
    return None


def save(value: str, state_dir: Path) -> None:
    if sys.platform == "darwin":
        if _keychain_add(value):
            return
        # Fall through to file storage if keychain is unavailable.
    path = _file_path(state_dir)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(value, encoding="utf-8")
    os.chmod(path, 0o600)


def delete(state_dir: Path) -> None:
    if sys.platform == "darwin":
        _keychain_delete()
    path = _file_path(state_dir)
    if path.exists():
        path.unlink()


def _file_path(state_dir: Path) -> Path:
    return state_dir / "daemon" / "webhook-secret"


def _keychain_find() -> str | None:
    try:
        result = subprocess.run(
            ["security", "find-generic-password", "-s", SERVICE, "-a", ACCOUNT, "-w"],
            capture_output=True,
            check=False,
            timeout=5,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return None
    if result.returncode != 0:
        return None
    value = result.stdout.decode(errors="replace").strip()
    return value or None


def _keychain_add(value: str) -> bool:
    try:
        subprocess.run(
            [
                "security",
                "add-generic-password",
                "-U",  # update if exists
                "-s",
                SERVICE,
                "-a",
                ACCOUNT,
                "-w",
                value,
            ],
            check=True,
            capture_output=True,
            timeout=5,
        )
        return True
    except (FileNotFoundError, subprocess.TimeoutExpired, subprocess.CalledProcessError):
        return False


def _keychain_delete() -> None:
    with contextlib.suppress(FileNotFoundError, subprocess.TimeoutExpired):
        subprocess.run(
            ["security", "delete-generic-password", "-s", SERVICE, "-a", ACCOUNT],
            check=False,
            capture_output=True,
            timeout=5,
        )
