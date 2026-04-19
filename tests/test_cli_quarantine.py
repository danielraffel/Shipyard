"""CLI tests for `shipyard quarantine {list,add,remove}`."""

from __future__ import annotations

import json
from pathlib import Path

from click.testing import CliRunner

from shipyard.cli import main
from shipyard.core.config import Config


def _config(tmp_path: Path) -> Config:
    project_dir = tmp_path / ".shipyard"
    project_dir.mkdir()
    return Config(
        data={
            "project": {"name": "shipyard"},
            "targets": {
                "mac": {"backend": "local", "platform": "macos-arm64"},
                "flaky-win": {"backend": "ssh", "platform": "win-x64", "host": "x"},
            },
        },
        project_dir=project_dir,
    )


def test_quarantine_list_empty(tmp_path, monkeypatch) -> None:
    config = _config(tmp_path)
    monkeypatch.setattr(
        "shipyard.cli.Config.load_from_cwd", lambda cwd=None: config
    )
    monkeypatch.setattr(
        "shipyard.core.config._default_state_dir", lambda: tmp_path / "state"
    )

    runner = CliRunner()
    result = runner.invoke(main, ["--json", "quarantine", "list"])
    assert result.exit_code == 0, result.output
    payload = json.loads(result.output)
    assert payload["entries"] == []
    assert payload["command"] == "quarantine.list"


def test_quarantine_add_and_list_roundtrip(tmp_path, monkeypatch) -> None:
    config = _config(tmp_path)
    monkeypatch.setattr(
        "shipyard.cli.Config.load_from_cwd", lambda cwd=None: config
    )
    monkeypatch.setattr(
        "shipyard.core.config._default_state_dir", lambda: tmp_path / "state"
    )

    runner = CliRunner()
    add = runner.invoke(
        main,
        ["--json", "quarantine", "add", "flaky-win", "--reason", "bad runner"],
    )
    assert add.exit_code == 0, add.output
    add_payload = json.loads(add.output)
    assert add_payload["added"] is True

    # File should exist on disk.
    q_file = config.project_dir / "quarantine.toml"
    assert q_file.exists()
    text = q_file.read_text()
    assert "flaky-win" in text
    assert "bad runner" in text

    # Re-adding the same target is a no-op.
    again = runner.invoke(
        main, ["--json", "quarantine", "add", "flaky-win"],
    )
    assert again.exit_code == 0
    assert json.loads(again.output)["added"] is False

    # List reflects the entry.
    listed = runner.invoke(main, ["--json", "quarantine", "list"])
    payload = json.loads(listed.output)
    entries = payload["entries"]
    assert [e["target"] for e in entries] == ["flaky-win"]
    assert entries[0]["reason"] == "bad runner"


def test_quarantine_remove(tmp_path, monkeypatch) -> None:
    config = _config(tmp_path)
    monkeypatch.setattr(
        "shipyard.cli.Config.load_from_cwd", lambda cwd=None: config
    )
    monkeypatch.setattr(
        "shipyard.core.config._default_state_dir", lambda: tmp_path / "state"
    )

    runner = CliRunner()
    runner.invoke(main, ["quarantine", "add", "flaky-win"])
    removed = runner.invoke(
        main, ["--json", "quarantine", "remove", "flaky-win"]
    )
    assert removed.exit_code == 0
    assert json.loads(removed.output)["removed"] is True

    # Removing again is an idempotent no-op.
    missing = runner.invoke(
        main, ["--json", "quarantine", "remove", "flaky-win"]
    )
    assert missing.exit_code == 0
    assert json.loads(missing.output)["removed"] is False


def test_quarantine_requires_project_dir(tmp_path, monkeypatch) -> None:
    # Config without project_dir — emulates running outside a
    # shipyard project.
    config = Config(data={}, project_dir=None)
    monkeypatch.setattr(
        "shipyard.cli.Config.load_from_cwd", lambda cwd=None: config
    )
    monkeypatch.setattr(
        "shipyard.core.config._default_state_dir", lambda: tmp_path / "state"
    )
    runner = CliRunner()
    result = runner.invoke(main, ["quarantine", "add", "foo"])
    assert result.exit_code != 0
