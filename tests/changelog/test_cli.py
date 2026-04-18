"""CLI tests — ``changelog`` group and ``release-bot hook`` subgroup."""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

import pytest
from click.testing import CliRunner

from shipyard.cli import main

from tests.changelog.conftest import commit, seed_repo, tag


def _write_config(repo: Path, body: str) -> None:
    (repo / ".shipyard").mkdir(exist_ok=True)
    (repo / ".shipyard" / "config.toml").write_text(body)


def _minimal_cfg(repo_url: str = "https://github.com/a/b") -> str:
    return (
        "[release.changelog]\n"
        "enabled    = true\n"
        f'repo_url   = "{repo_url}"\n'
        'tag_filter = "v*"\n'
        'product    = "Sample"\n'
        "\n"
        "[release.post_tag_hook]\n"
        "enabled = true\n"
        'command = "true"\n'
        'watch = ["CHANGELOG.md"]\n'
    )


@pytest.fixture
def cli_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    repo = seed_repo(tmp_path / "repo")
    monkeypatch.chdir(repo)
    commit(repo, "a.txt", "a", "feat: a (#1)")
    tag(repo, "v0.1.0")
    commit(repo, "b.txt", "b", "feat: b (#2)")
    tag(repo, "v0.2.0")
    return repo


def test_regenerate_writes_file(cli_repo: Path) -> None:
    _write_config(cli_repo, _minimal_cfg())
    runner = CliRunner()
    res = runner.invoke(main, ["--json", "changelog", "regenerate"])
    assert res.exit_code == 0, res.output
    payload = json.loads(res.output)
    assert payload["command"] == "changelog:regenerate"
    assert payload["versions"] == 2
    assert (cli_repo / "CHANGELOG.md").exists()


def test_regenerate_missing_config_exits_2(cli_repo: Path) -> None:
    # no .shipyard/config.toml
    runner = CliRunner()
    res = runner.invoke(main, ["--json", "changelog", "regenerate"])
    assert res.exit_code == 2
    # Error rendered as JSON.
    payload = json.loads(res.output)
    assert payload["command"] == "changelog:error"
    assert payload["error"] == "disabled"


def test_check_detects_drift(cli_repo: Path) -> None:
    _write_config(cli_repo, _minimal_cfg())
    (cli_repo / "CHANGELOG.md").write_text("stale\n")
    runner = CliRunner()
    res = runner.invoke(main, ["--json", "changelog", "check"])
    assert res.exit_code == 1
    payload = json.loads(res.output)
    assert payload["drift"] is True


def test_check_reports_in_sync(cli_repo: Path) -> None:
    _write_config(cli_repo, _minimal_cfg())
    # Bring CHANGELOG into sync first.
    runner = CliRunner()
    assert runner.invoke(main, ["changelog", "regenerate"]).exit_code == 0
    res = runner.invoke(main, ["--json", "changelog", "check"])
    assert res.exit_code == 0
    payload = json.loads(res.output)
    assert payload["drift"] is False


def test_release_notes_stdout(cli_repo: Path) -> None:
    _write_config(cli_repo, _minimal_cfg())
    runner = CliRunner()
    res = runner.invoke(main, ["changelog", "regenerate", "--release-notes", "v0.2.0"])
    assert res.exit_code == 0
    assert "What's new in v0.2.0" in res.output


def test_release_notes_unknown_tag_exits_2(cli_repo: Path) -> None:
    _write_config(cli_repo, _minimal_cfg())
    runner = CliRunner()
    res = runner.invoke(
        main, ["--json", "changelog", "regenerate", "--release-notes", "v9.9.9"]
    )
    assert res.exit_code == 2


def test_init_creates_config_and_backs_up_changelog(cli_repo: Path) -> None:
    (cli_repo / "CHANGELOG.md").write_text("hand-written notes\n")
    runner = CliRunner()
    res = runner.invoke(
        main,
        [
            "--json",
            "changelog",
            "init",
            "--repo-url",
            "https://github.com/a/b",
            "--product",
            "Sample",
        ],
    )
    assert res.exit_code == 0, res.output
    payload = json.loads(res.output)
    assert payload["status"] == "written"
    assert payload["existing_changelog"] is True
    backup = Path(payload["changelog_backup"])
    assert backup.exists()
    assert backup.read_text() == "hand-written notes\n"

    cfg_body = (cli_repo / ".shipyard" / "config.toml").read_text()
    assert "[release.changelog]" in cfg_body
    assert "[release.post_tag_hook]" in cfg_body


def test_init_already_configured_is_noop(cli_repo: Path) -> None:
    _write_config(cli_repo, _minimal_cfg())
    runner = CliRunner()
    res = runner.invoke(main, ["--json", "changelog", "init"])
    assert res.exit_code == 0
    payload = json.loads(res.output)
    assert payload["status"] == "already_configured"


def test_hook_install_writes_workflow(cli_repo: Path) -> None:
    runner = CliRunner()
    res = runner.invoke(
        main,
        [
            "--json",
            "release-bot",
            "hook",
            "install",
            "--tag-pattern",
            "v*",
            "--shipyard-version",
            "0.9.0",
        ],
    )
    assert res.exit_code == 0, res.output
    payload = json.loads(res.output)
    assert payload["overwrote"] is False
    workflow = cli_repo / ".github" / "workflows" / "post-tag-sync.yml"
    assert workflow.exists()
    body = workflow.read_text()
    assert 'SHIPYARD_VERSION: "0.9.0"' in body
    assert "shipyard release-bot hook run" in body


def test_hook_install_is_idempotent(cli_repo: Path) -> None:
    runner = CliRunner()
    assert runner.invoke(
        main, ["release-bot", "hook", "install"]
    ).exit_code == 0
    res = runner.invoke(main, ["--json", "release-bot", "hook", "install"])
    payload = json.loads(res.output)
    assert payload["overwrote"] is True


def test_hook_run_without_config_is_skip(cli_repo: Path) -> None:
    runner = CliRunner()
    res = runner.invoke(
        main, ["--json", "release-bot", "hook", "run", "--tag", "v0.2.0"]
    )
    assert res.exit_code == 0
    payload = json.loads(res.output)
    assert payload["skipped_reason"] is not None
