"""Create / update / delete GitHub repository webhooks via ``gh``.

Mirrors ``WebhookRegistrar.swift``. Uses the user's existing ``gh``
auth — no extra token plumbing. Hook IDs persist to disk so a second
launch can find + reuse an existing registration instead of creating
duplicates.
"""

from __future__ import annotations

import asyncio
import json
import logging
import os
import shutil
from dataclasses import dataclass
from pathlib import Path

logger = logging.getLogger(__name__)

SUBSCRIBED_EVENTS = (
    "workflow_run",
    "workflow_job",
    "pull_request",
    "check_run",
    "check_suite",
    "release",
)


class RegistrarError(Exception):
    """Non-recoverable failure talking to ``gh api``."""


@dataclass
class RegisteredHook:
    repo: str
    hook_id: int


class Registrar:
    """In-memory map of registered hooks, persisted between runs."""

    def __init__(self, state_dir: Path) -> None:
        self._state_path = state_dir / "daemon" / "registrations.json"
        self._by_repo: dict[str, int] = {}
        self._load()

    def all(self) -> dict[str, int]:
        return dict(self._by_repo)

    async def ensure_registered(
        self,
        repo: str,
        url: str,
        secret: str,
        *,
        gh_binary: str | None = None,
    ) -> int:
        """Idempotent create-or-update. Returns the hook ID."""
        binary = gh_binary or shutil.which("gh")
        if binary is None:
            raise RegistrarError("gh CLI not found on PATH")
        if not os.access(binary, os.X_OK) or not Path(binary).is_file():
            raise RegistrarError(f"gh CLI not executable: {binary}")
        if repo in self._by_repo:
            # Already registered — update the URL/secret in case they
            # rotated since last time.
            existing_id = self._by_repo[repo]
            await self._update(binary, repo, existing_id, url, secret)
            return existing_id
        hook_id = await self._create(binary, repo, url, secret)
        self._by_repo[repo] = hook_id
        self._save()
        return hook_id

    async def unregister(
        self,
        repo: str,
        *,
        gh_binary: str | None = None,
    ) -> None:
        if repo not in self._by_repo:
            return
        hook_id = self._by_repo[repo]
        binary = gh_binary or shutil.which("gh")
        if binary is not None:
            await self._delete(binary, repo, hook_id)
        del self._by_repo[repo]
        self._save()

    async def unregister_all(self, *, gh_binary: str | None = None) -> None:
        for repo in list(self._by_repo):
            await self.unregister(repo, gh_binary=gh_binary)

    # --- internals -------------------------------------------------

    async def _create(
        self, binary: str, repo: str, url: str, secret: str
    ) -> int:
        config = {
            "url": url,
            "content_type": "json",
            "secret": secret,
            "insecure_ssl": "0",
        }
        body = {
            "name": "web",
            "active": True,
            "events": list(SUBSCRIBED_EVENTS),
            "config": config,
        }
        code, out = await _run_gh(
            binary,
            [
                "api",
                "-X",
                "POST",
                "-H",
                "Accept: application/vnd.github+json",
                "--input",
                "-",
                f"repos/{repo}/hooks",
            ],
            stdin=json.dumps(body).encode(),
        )
        if code != 0:
            raise RegistrarError(f"create hook failed: {out.strip()}")
        try:
            parsed = json.loads(out)
            hook_id = int(parsed["id"])
        except (json.JSONDecodeError, KeyError, TypeError, ValueError) as exc:
            raise RegistrarError(
                f"couldn't parse hook ID from gh response: {out.strip()}"
            ) from exc
        return hook_id

    async def _update(
        self, binary: str, repo: str, hook_id: int, url: str, secret: str
    ) -> None:
        body = {
            "config": {
                "url": url,
                "content_type": "json",
                "secret": secret,
                "insecure_ssl": "0",
            },
            "active": True,
        }
        code, out = await _run_gh(
            binary,
            [
                "api",
                "-X",
                "PATCH",
                "-H",
                "Accept: application/vnd.github+json",
                "--input",
                "-",
                f"repos/{repo}/hooks/{hook_id}",
            ],
            stdin=json.dumps(body).encode(),
        )
        if code != 0:
            raise RegistrarError(f"patch hook failed: {out.strip()}")

    async def _delete(self, binary: str, repo: str, hook_id: int) -> None:
        code, out = await _run_gh(
            binary,
            ["api", "-X", "DELETE", f"repos/{repo}/hooks/{hook_id}"],
        )
        if code == 0:
            return
        lowered = out.lower()
        if "404" in lowered or "not found" in lowered:
            return
        raise RegistrarError(f"delete hook failed: {out.strip()}")

    def _load(self) -> None:
        if not self._state_path.exists():
            return
        try:
            data = json.loads(self._state_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            return
        if isinstance(data, list):
            for item in data:
                if isinstance(item, dict) and "repo" in item and "hook_id" in item:
                    self._by_repo[str(item["repo"])] = int(item["hook_id"])

    def _save(self) -> None:
        self._state_path.parent.mkdir(parents=True, exist_ok=True)
        payload = [
            {"repo": repo, "hook_id": hook_id}
            for repo, hook_id in sorted(self._by_repo.items())
        ]
        self._state_path.write_text(
            json.dumps(payload, indent=2), encoding="utf-8"
        )


async def _run_gh(
    binary: str,
    args: list[str],
    *,
    stdin: bytes | None = None,
) -> tuple[int, str]:
    proc = await asyncio.create_subprocess_exec(
        binary,
        *args,
        stdin=asyncio.subprocess.PIPE if stdin else None,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    stdout, _ = await proc.communicate(input=stdin)
    return proc.returncode or 0, (stdout or b"").decode(errors="replace")
