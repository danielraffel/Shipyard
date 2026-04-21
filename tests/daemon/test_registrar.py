"""Registrar tests — ``gh`` is faked with a tiny shell stub.

The stub reads stdin, prints a canned response, exits 0 unless the
caller asked for a failure. This exercises the registrar's subprocess
handling without requiring a real ``gh`` binary or real GitHub auth.
"""

from __future__ import annotations

import asyncio
import json
import os
import stat
from pathlib import Path

import pytest

from shipyard.daemon.registrar import Registrar, RegistrarError


def _write_gh_stub(path: Path, *, hook_id: int = 42, fail: bool = False) -> str:
    """Write a tiny script that behaves like ``gh api``."""
    script = path / "gh"
    if fail:
        body = "#!/usr/bin/env bash\necho 'gh: simulated failure' >&2\nexit 1\n"
    else:
        body = (
            "#!/usr/bin/env bash\n"
            f"echo '{{\"id\": {hook_id}, \"active\": true}}'\n"
            "exit 0\n"
        )
    script.write_text(body)
    os.chmod(script, os.stat(script).st_mode | stat.S_IEXEC | stat.S_IXGRP | stat.S_IXOTH)
    return str(script)


def test_create_caches_hook_id(tmp_path: Path) -> None:
    gh = _write_gh_stub(tmp_path, hook_id=9001)
    registrar = Registrar(state_dir=tmp_path)

    hook_id = asyncio.run(
        registrar.ensure_registered(
            "org/repo",
            "https://example.ts.net/webhook",
            "secret",
            gh_binary=gh,
        )
    )
    assert hook_id == 9001
    # Second call reuses the cached ID — subsequent calls go through
    # the update (PATCH) path, not create, so it's bound to the same
    # hook ID regardless.
    reused = asyncio.run(
        registrar.ensure_registered(
            "org/repo",
            "https://example.ts.net/webhook",
            "secret",
            gh_binary=gh,
        )
    )
    assert reused == 9001


def test_create_persists_to_disk_across_instances(tmp_path: Path) -> None:
    gh = _write_gh_stub(tmp_path, hook_id=7)

    first = Registrar(state_dir=tmp_path)
    hook_id = asyncio.run(
        first.ensure_registered("org/repo", "https://x.ts.net/webhook", "s", gh_binary=gh)
    )
    assert hook_id == 7

    # New Registrar instance reads the persisted file.
    second = Registrar(state_dir=tmp_path)
    assert second.all() == {"org/repo": 7}


def test_create_failure_bubbles_as_registrar_error(tmp_path: Path) -> None:
    gh = _write_gh_stub(tmp_path, fail=True)
    registrar = Registrar(state_dir=tmp_path)

    with pytest.raises(RegistrarError):
        asyncio.run(
            registrar.ensure_registered(
                "org/repo", "https://x.ts.net/webhook", "s", gh_binary=gh
            )
        )


def test_missing_gh_binary_raises(tmp_path: Path) -> None:
    registrar = Registrar(state_dir=tmp_path)
    with pytest.raises(RegistrarError):
        asyncio.run(
            registrar.ensure_registered(
                "org/repo",
                "https://x.ts.net/webhook",
                "s",
                gh_binary=str(tmp_path / "does-not-exist"),
            )
        )


def test_unregister_removes_from_persistence(tmp_path: Path) -> None:
    gh = _write_gh_stub(tmp_path, hook_id=5)
    registrar = Registrar(state_dir=tmp_path)

    asyncio.run(
        registrar.ensure_registered(
            "org/repo", "https://x.ts.net/webhook", "s", gh_binary=gh
        )
    )
    assert registrar.all() == {"org/repo": 5}

    # Unregister — gh DELETE returns 0 from our stub.
    asyncio.run(registrar.unregister("org/repo", gh_binary=gh))
    assert registrar.all() == {}

    # Persisted file reflects the empty state.
    persisted = json.loads(
        (tmp_path / "daemon" / "registrations.json").read_text()
    )
    assert persisted == []
