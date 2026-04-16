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

    def test_dirty_index_refuses(self, tmp_repo: Path) -> None:
        # #59 P1: staged changes would be folded into the amend.
        # We must refuse rather than surprise.
        (tmp_repo / "b.txt").write_text("b\n")
        subprocess.check_call(["git", "add", "b.txt"], cwd=tmp_repo)
        with pytest.raises(_TrailerAmendError) as exc:
            _append_trailers_to_tip(['Version-Bump: sdk=skip reason="x"'])
        assert "staged" in str(exc.value).lower()
        # And the index is still intact for the user to deal with.
        result = subprocess.run(
            ["git", "diff", "--cached", "--quiet"],
            cwd=tmp_repo,
        )
        assert result.returncode != 0

    def test_replaces_stale_version_bump_same_surface(
        self, tmp_repo: Path
    ) -> None:
        # #59 P2: if HEAD already has Version-Bump: sdk=patch, adding
        # Version-Bump: sdk=skip must REPLACE the old line, not stack.
        # Otherwise scripts/version_bump_check.py's first-match logic
        # would pick up the stale value.
        subprocess.check_call(
            ["git", "commit", "--allow-empty", "-m",
             "seed\n\nVersion-Bump: sdk=patch reason=\"old\""],
            cwd=tmp_repo,
        )
        _append_trailers_to_tip(['Version-Bump: sdk=skip reason="new"'])
        body = _tip_body()
        # Stale one gone.
        assert 'Version-Bump: sdk=patch' not in body
        # New one present.
        assert 'Version-Bump: sdk=skip reason="new"' in body
        # Only one Version-Bump line for sdk.
        assert body.count("Version-Bump: sdk") == 1

    def test_keeps_version_bump_for_different_surface(
        self, tmp_repo: Path
    ) -> None:
        # Strip should NOT touch a Version-Bump for a different surface.
        subprocess.check_call(
            ["git", "commit", "--allow-empty", "-m",
             "seed\n\nVersion-Bump: cli=minor reason=\"feat\""],
            cwd=tmp_repo,
        )
        _append_trailers_to_tip(['Version-Bump: sdk=skip reason="docs"'])
        body = _tip_body()
        assert 'Version-Bump: cli=minor reason="feat"' in body
        assert 'Version-Bump: sdk=skip reason="docs"' in body

    def test_replaces_stale_skill_update_same_skill(
        self, tmp_repo: Path
    ) -> None:
        subprocess.check_call(
            ["git", "commit", "--allow-empty", "-m",
             "seed\n\nSkill-Update: skip skill=ci reason=\"old\""],
            cwd=tmp_repo,
        )
        _append_trailers_to_tip(
            ['Skill-Update: skip skill=ci reason="new"']
        )
        body = _tip_body()
        assert 'reason="old"' not in body
        assert 'reason="new"' in body
        assert body.count("Skill-Update: skip skill=ci") == 1

    def test_keeps_skill_update_for_different_skill(
        self, tmp_repo: Path
    ) -> None:
        subprocess.check_call(
            ["git", "commit", "--allow-empty", "-m",
             "seed\n\nSkill-Update: skip skill=api reason=\"x\""],
            cwd=tmp_repo,
        )
        _append_trailers_to_tip(
            ['Skill-Update: skip skill=ci reason="y"']
        )
        body = _tip_body()
        assert 'skill=api' in body
        assert 'skill=ci' in body


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
