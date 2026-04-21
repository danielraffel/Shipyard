"""Tests for `_compose_pr_body` — the PR body that lands on every new
PR opened by `shipyard ship`.

Historical behavior: the body was literally `Automated by Shipyard.`
with an optional advisory-lanes section. Not useful to reviewers and
leaked Shipyard branding into a consumer-visible surface.

Post-fix: the body is primarily the tip commit body — the author's
own description of the change — with a thin footer for validation
targets + watch pointer. Bypass trailers get a `[!WARNING]` callout
so reviewers can't miss an override of a default gate.
"""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import pytest

from shipyard.cli import (
    _collect_bypass_trailers,
    _compose_pr_body,
    _git_commit_body,
)
from shipyard.core.config import Config


# ── Helpers ─────────────────────────────────────────────────────


def _init_repo_with_commit(tmp_path: Path, commit_msg: str) -> str:
    """Make a one-commit repo with `commit_msg` as the full message.
    Returns the tip SHA. Repo path is `tmp_path/repo`; idempotent on
    re-init of the same tmp_path."""
    repo = tmp_path / "repo"
    repo.mkdir(exist_ok=True)
    subprocess.run(
        ["git", "init", "-q", "-b", "main"], cwd=repo, check=True,
    )
    subprocess.run(
        ["git", "-c", "user.email=t@t", "-c", "user.name=t",
         "commit", "--allow-empty", "-m", commit_msg],
        cwd=repo, check=True, capture_output=True,
    )
    sha = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=repo, text=True,
    ).strip()
    return sha


def _config(targets: dict[str, Any]) -> Config:
    return Config(data={"project": {"name": "demo"}, "targets": targets})


# ── _git_commit_body ────────────────────────────────────────────


class TestGitCommitBody:
    def test_subject_only_commit_returns_empty(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "subject only")
        monkeypatch.chdir(tmp_path / "repo")
        assert _git_commit_body(sha) == ""

    def test_subject_plus_body_returns_body(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(
            tmp_path,
            "fix: atomic queue writes\n\n"
            "The queue writer wrote directly to queue.json.\n"
            "A kill mid-write produced a zero-byte file.\n",
        )
        monkeypatch.chdir(tmp_path / "repo")
        body = _git_commit_body(sha)
        assert "queue writer wrote directly" in body
        assert "zero-byte file" in body
        # Subject must NOT be in the body — it's the title.
        assert "fix: atomic queue writes" not in body

    def test_trailer_block_stripped_from_body(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(
            tmp_path,
            "feat: add thing\n\n"
            "Body paragraph explaining thing.\n\n"
            "Version-Bump: cli=patch reason=\"bug fix only\"\n"
            "Skill-Update: skip skill=ci reason=\"mechanical\"\n",
        )
        monkeypatch.chdir(tmp_path / "repo")
        body = _git_commit_body(sha)
        assert "Body paragraph" in body
        # Shipyard-recognized trailers belong in the bypass callout,
        # not the body. (Git's own trailer detector requires contiguous
        # Key:Value lines at the end — GitHub's inline `Closes #N`
        # form isn't a real trailer and stays in the body, which is
        # fine: it's part of the author's description.)
        assert "Version-Bump" not in body
        assert "Skill-Update" not in body


# ── _collect_bypass_trailers ────────────────────────────────────


class TestCollectBypassTrailers:
    def test_no_trailers_returns_empty_list(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "fix: plain\n\nBody.\n")
        monkeypatch.chdir(tmp_path / "repo")
        assert _collect_bypass_trailers(sha) == []

    def test_non_bypass_trailers_ignored(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Closes / Co-Authored-By / Signed-Off-By are real trailers
        but don't disable a gate, so they don't warrant a callout."""
        sha = _init_repo_with_commit(
            tmp_path,
            "fix: x\n\nBody.\n\n"
            "Closes #10\n"
            "Co-Authored-By: Someone <s@example.com>\n"
            "Signed-Off-By: Me <m@example.com>\n",
        )
        monkeypatch.chdir(tmp_path / "repo")
        assert _collect_bypass_trailers(sha) == []

    def test_version_bump_skip_is_flagged(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(
            tmp_path,
            "chore: doc tweak\n\nBody.\n\n"
            'Version-Bump: cli=skip reason="docs only"\n',
        )
        monkeypatch.chdir(tmp_path / "repo")
        trailers = _collect_bypass_trailers(sha)
        assert len(trailers) == 1
        assert "Version-Bump:" in trailers[0]
        assert "cli=skip" in trailers[0]

    def test_mixed_bypasses_preserved_in_order(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(
            tmp_path,
            "feat: big thing\n\nBody.\n\n"
            'Version-Bump: cli=major reason="breaking"\n'
            'Skill-Update: skip skill=ci reason="mechanical"\n'
            'Lane-Policy: windows=advisory\n'
            'Release: skip reason="batched"\n',
        )
        monkeypatch.chdir(tmp_path / "repo")
        trailers = _collect_bypass_trailers(sha)
        assert len(trailers) == 4
        keys = [t.partition(":")[0].lower() for t in trailers]
        assert keys == [
            "version-bump", "skill-update", "lane-policy", "release"
        ]


# ── _compose_pr_body integration ────────────────────────────────


class TestComposePrBody:
    def test_body_is_primarily_commit_body(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(
            tmp_path,
            "fix: robust Windows SSH probe\n\n"
            "The Windows probe previously called powershell without "
            "BatchMode=yes. Hung on slow handshakes.\n",
        )
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({"macos": {"platform": "macos-arm64"}}),
            sha=sha,
        )
        # Commit body MUST be the primary content, unwrapped.
        assert "Windows probe previously called powershell" in body
        assert "slow handshakes" in body
        # And NOT the banned phrases.
        assert "Automated by Shipyard" not in body

    def test_subject_only_commit_gets_placeholder(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "fix: tiny")
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({"mac": {"platform": "macos-arm64"}}),
            sha=sha,
        )
        # Placeholder prompts the author to add context.
        assert "no commit body" in body.lower()
        assert "reviewers know" in body.lower()

    def test_bypass_trailer_surfaces_as_warning_callout(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(
            tmp_path,
            "chore: docs\n\nBody.\n\n"
            'Version-Bump: cli=skip reason="docs only"\n',
        )
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({"mac": {"platform": "macos-arm64"}}),
            sha=sha,
        )
        # GitHub native callout — the WARNING is what makes it pop.
        assert "> [!WARNING]" in body
        assert "Bypasses on this PR" in body
        assert "Version-Bump: cli=skip" in body

    def test_no_bypass_means_no_warning_block(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "fix: plain\n\nBody.\n")
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({"mac": {"platform": "macos-arm64"}}),
            sha=sha,
        )
        # Don't render the warning block when there's nothing to warn about.
        assert "[!WARNING]" not in body
        assert "Bypasses" not in body

    def test_footer_lists_targets_in_config_order(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "fix: x\n\nBody.\n")
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({
                "macos": {"platform": "macos-arm64"},
                "ubuntu": {"platform": "linux-x64"},
                "windows": {"platform": "windows-x64"},
            }),
            sha=sha,
        )
        assert "Validation:" in body
        assert "`macos`" in body
        assert "`ubuntu`" in body
        assert "`windows`" in body
        assert "Follow: `shipyard watch`" in body
        # Footer wrapped in <sub> so it's visually subordinate.
        assert "<sub>" in body

    def test_footer_omits_validation_line_when_no_targets(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "fix: x\n\nBody.\n")
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(_config({}), sha=sha)
        # No targets configured → don't invent a fake validation line.
        assert "Validation:" not in body
        # Commit body still there.
        assert "Body" in body

    def test_advisory_lane_annotated_in_footer(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        sha = _init_repo_with_commit(tmp_path, "fix: x\n\nBody.\n")
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({
                "macos": {"platform": "macos-arm64"},
                "ubuntu": {"platform": "linux-x64", "advisory": True},
            }),
            sha=sha,
        )
        # Advisory rendered inline in the footer, not as its own H2.
        assert "Advisory: `ubuntu`" in body
        # The old "## Advisory lanes" H2 must be gone.
        assert "## Advisory lanes" not in body

    def test_no_ship_terminology_in_output(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Feedback: 'ship' is internal terminology. The PR body is a
        consumer-visible surface — GitHub reviewers see it. Keep the
        word out of the default template."""
        sha = _init_repo_with_commit(tmp_path, "fix: x\n\nBody.\n")
        monkeypatch.chdir(tmp_path / "repo")
        body = _compose_pr_body(
            _config({"mac": {"platform": "macos-arm64"}}),
            sha=sha,
        )
        # Case-insensitive: "Ship", "SHIP", " ship " all disallowed.
        # `shipyard watch` is a command name — fine as backtick-wrapped
        # text, so we only flag the standalone word.
        import re
        stand_alone_ship = re.compile(r"\bship\b", re.IGNORECASE)
        # The footer includes `shipyard watch` but not a bare "ship".
        assert not stand_alone_ship.search(body), (
            f"bare 'ship' word leaked into PR body:\n{body}"
        )
