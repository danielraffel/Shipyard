"""Tests for trailer shortcut helpers and `shipyard pr` flag plumbing.

The amend path (_append_trailers_to_tip) is exercised against a
real temporary git repository so we catch any interaction between
git interpret-trailers, `git commit --amend`, and our message
handling. The CLI flag wiring is checked by patching the helper
out to a fake and asserting it was invoked correctly.
"""

from __future__ import annotations

import subprocess
from typing import TYPE_CHECKING

import pytest

from shipyard.cli import _append_trailers_to_tip, _TrailerAmendError

if TYPE_CHECKING:
    from pathlib import Path


@pytest.fixture
def tmp_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Initialize a throwaway git repo and chdir there for the test."""
    monkeypatch.chdir(tmp_path)
    monkeypatch.setenv("GIT_AUTHOR_NAME", "Test")
    monkeypatch.setenv("GIT_AUTHOR_EMAIL", "test@example.com")
    monkeypatch.setenv("GIT_COMMITTER_NAME", "Test")
    monkeypatch.setenv("GIT_COMMITTER_EMAIL", "test@example.com")
    subprocess.check_call(
        ["git", "init", "--quiet", "--initial-branch=main"],
        cwd=tmp_path,
    )
    (tmp_path / "a.txt").write_text("a\n")
    subprocess.check_call(["git", "add", "a.txt"], cwd=tmp_path)
    subprocess.check_call(
        ["git", "commit", "-m", "initial commit body"],
        cwd=tmp_path,
        stdout=subprocess.DEVNULL,
    )
    return tmp_path


def _tip_body() -> str:
    return subprocess.check_output(
        ["git", "log", "-1", "--format=%B"], text=True
    )


class TestAppendTrailers:
    def test_adds_trailer(self, tmp_repo: Path) -> None:
        added = _append_trailers_to_tip(
            ['Version-Bump: sdk=skip reason="docs only"']
        )
        assert added == ['Version-Bump: sdk=skip reason="docs only"']
        body = _tip_body()
        assert "initial commit body" in body
        assert 'Version-Bump: sdk=skip reason="docs only"' in body

    def test_idempotent_when_trailer_already_present(
        self, tmp_repo: Path
    ) -> None:
        trailer = 'Skill-Update: skip skill=ci reason="none"'
        first = _append_trailers_to_tip([trailer])
        assert first == [trailer]
        second = _append_trailers_to_tip([trailer])
        assert second == []  # nothing added the second time

    def test_multiple_trailers(self, tmp_repo: Path) -> None:
        trailers = [
            'Version-Bump: sdk=skip reason="r1"',
            'Skill-Update: skip skill=ci reason="r2"',
        ]
        added = _append_trailers_to_tip(trailers)
        assert added == trailers
        body = _tip_body()
        assert all(t in body for t in trailers)

    def test_preserves_existing_body(self, tmp_repo: Path) -> None:
        subprocess.check_call(
            ["git", "commit", "--allow-empty", "-m",
             "fancy commit\n\nWith body lines.\nAnd more."],
            stdout=subprocess.DEVNULL,
        )
        added = _append_trailers_to_tip(
            ['Version-Bump: cli=skip reason="x"']
        )
        assert len(added) == 1
        body = _tip_body()
        assert "With body lines." in body
        assert "And more." in body
        assert 'Version-Bump: cli=skip reason="x"' in body

    def test_empty_trailer_list_is_noop(self, tmp_repo: Path) -> None:
        assert _append_trailers_to_tip([]) == []

    def test_outside_repo_raises_typed_error(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        monkeypatch.chdir(tmp_path)
        with pytest.raises(_TrailerAmendError):
            _append_trailers_to_tip(['Version-Bump: sdk=skip reason="x"'])


class TestPrFlagWiring:
    """Prove the flags reach _append_trailers_to_tip with correctly-formed trailer strings."""

    def test_skip_bump_requires_reason(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from click.testing import CliRunner

        from shipyard.cli import main

        runner = CliRunner()
        result = runner.invoke(main, ["pr", "--skip-bump", "sdk"])
        assert result.exit_code == 2
        assert "--bump-reason" in result.output

    def test_skip_skill_update_requires_reason(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from click.testing import CliRunner

        from shipyard.cli import main

        runner = CliRunner()
        result = runner.invoke(
            main, ["pr", "--skip-skill-update", "ci"]
        )
        assert result.exit_code == 2
        assert "--skill-reason" in result.output

    def test_flags_append_expected_trailers(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from click.testing import CliRunner

        from shipyard.cli import main

        captured: list[list[str]] = []

        def fake_append(trailers: list[str]) -> list[str]:
            captured.append(list(trailers))
            return list(trailers)

        monkeypatch.setattr(
            "shipyard.cli._append_trailers_to_tip", fake_append
        )
        # Short-circuit the rest of the pr flow.
        monkeypatch.setattr(
            "shipyard.cli.Path.exists", lambda self: False
        )

        runner = CliRunner()
        result = runner.invoke(
            main,
            [
                "pr",
                "--skip-bump", "sdk",
                "--bump-reason", "docs only",
                "--skip-skill-update", "ci",
                "--skill-reason", "mechanical change",
            ],
        )
        # Exit code 2 (gate scripts missing) is fine — we only care
        # about the trailer plumbing, which runs before the gate check.
        assert captured, result.output
        assert captured[0] == [
            'Version-Bump: sdk=skip reason="docs only"',
            'Skill-Update: skip skill=ci reason="mechanical change"',
        ]
