"""Tests for ``shipyard daemon refresh`` (#231).

Drives the CLI with a mocked ``read_daemon_status`` + ``stop_running``
+ ``run_detached`` so the test doesn't need a real running daemon.
"""

from __future__ import annotations

import json
import os  # noqa: F401 — kept for symmetry with sibling test modules
import sys
from pathlib import Path  # noqa: TC003 — runtime use via tmp_path
from typing import Any

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixture
from click.testing import CliRunner

from shipyard.cli import main

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "#198: Click CliRunner isolation flake on Windows across "
        "this family of CLI tests. Coverage preserved on Linux + macOS."
    ),
)


def _assert_cli_ok(result: Any) -> None:
    assert result.exit_code == 0, (
        f"exit={result.exit_code} output={result.output!r} "
        f"exc={result.exception!r}"
    )


def _patch_daemon(
    monkeypatch: pytest.MonkeyPatch,
    *,
    prior_running: bool = True,
    prior_repos: list[str] | None = None,
) -> dict[str, Any]:
    """Install fakes for the three daemon-module hooks refresh uses.

    Returns a dict the test can inspect to confirm which calls
    happened in which order.
    """
    calls: dict[str, Any] = {
        "stop_called": False,
        "run_detached_repos": None,
        "run_detached_pid": 99999,
    }

    def fake_read_status(state_dir):
        if not prior_running:
            return None
        return {
            "shipyard_version": "0.38.0",
            "registered_repos": prior_repos or [],
        }

    def fake_stop(state_dir):
        calls["stop_called"] = True
        return prior_running

    def fake_spawn_detached(*, state_dir, repos):
        calls["run_detached_repos"] = list(repos)
        return calls["run_detached_pid"]

    monkeypatch.setattr(
        "shipyard.daemon.controller.read_daemon_status", fake_read_status,
    )
    monkeypatch.setattr(
        "shipyard.daemon.runner.stop_running", fake_stop,
    )
    monkeypatch.setattr(
        "shipyard.daemon.runner.spawn_detached", fake_spawn_detached,
    )
    return calls


def test_refresh_restarts_with_explicit_repos(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    calls = _patch_daemon(
        monkeypatch, prior_running=True, prior_repos=["owner/a"],
    )
    runner = CliRunner()
    result = runner.invoke(
        main,
        ["daemon", "refresh", "--repo", "owner/b", "--repo", "owner/c"],
    )
    _assert_cli_ok(result)
    assert calls["stop_called"] is True
    # Explicit --repo wins over whatever the prior daemon had.
    assert calls["run_detached_repos"] == ["owner/b", "owner/c"]


def test_refresh_reuses_prior_repos_when_none_passed(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    # Load-bearing UX: operators shouldn't have to remember which
    # repos their daemon was serving. refresh reuses the prior set.
    calls = _patch_daemon(
        monkeypatch,
        prior_running=True,
        prior_repos=["owner/x", "owner/y"],
    )
    runner = CliRunner()
    result = runner.invoke(main, ["daemon", "refresh"])
    _assert_cli_ok(result)
    assert calls["run_detached_repos"] == ["owner/x", "owner/y"]


def test_refresh_without_running_daemon_starts_fresh(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    # If nothing's running, refresh still starts a fresh daemon —
    # idempotent. The stop step runs but returns False (nothing to
    # stop). Message text should reflect that.
    _patch_daemon(
        monkeypatch, prior_running=False,
    )
    runner = CliRunner()
    result = runner.invoke(
        main, ["daemon", "refresh", "--repo", "owner/a"],
    )
    _assert_cli_ok(result)
    assert "no prior daemon" in result.output.lower() or \
           "started fresh" in result.output.lower()


def test_refresh_refuses_when_no_repos_available(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    # Prior daemon had no repos AND caller didn't pass --repo. We
    # can't cleanly spawn a daemon with no repos registered, so
    # refuse loudly and tell the operator how to recover.
    _patch_daemon(
        monkeypatch, prior_running=True, prior_repos=[],
    )
    runner = CliRunner()
    result = runner.invoke(main, ["daemon", "refresh"])
    assert result.exit_code != 0
    assert "--repo" in result.output


def test_refresh_emits_json_envelope(
    monkeypatch: pytest.MonkeyPatch, tmp_path: Path,
) -> None:
    # Agent workflows (hooks, CI) need a parseable envelope.
    _patch_daemon(
        monkeypatch, prior_running=True, prior_repos=["owner/a"],
    )
    runner = CliRunner()
    result = runner.invoke(
        main, ["--json", "daemon", "refresh", "--repo", "owner/a"],
    )
    _assert_cli_ok(result)
    parsed = json.loads(result.output)
    assert parsed["command"] == "daemon:refresh"
    assert parsed["stopped_prior"] is True
    assert parsed["new_pid"] == 99999
    assert parsed["repos"] == ["owner/a"]
