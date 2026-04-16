"""Tests for `shipyard cloud run --require-sha`."""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from shipyard.cli import _resolve_expected_sha

if TYPE_CHECKING:
    import pytest


class TestResolveExpectedSHA:
    def test_head_uses_git(self, monkeypatch: pytest.MonkeyPatch) -> None:
        class FakeResult:
            returncode = 0
            stdout = "abcd1234" * 5 + "\n"  # 40 hex chars

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: FakeResult()
        )
        assert _resolve_expected_sha("HEAD") == "abcd1234" * 5

    def test_head_failure_returns_none(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        class FakeResult:
            returncode = 1
            stdout = ""

        monkeypatch.setattr(
            "shipyard.cli.subprocess.run", lambda *a, **kw: FakeResult()
        )
        assert _resolve_expected_sha("HEAD") is None

    def test_40_char_sha_returned(self) -> None:
        full = "a" * 40
        assert _resolve_expected_sha(full) == full

    def test_mixed_case_lowered(self) -> None:
        assert _resolve_expected_sha("A" * 40) == "a" * 40

    def test_short_sha_rejected(self) -> None:
        assert _resolve_expected_sha("abc1234") is None

    def test_non_hex_rejected(self) -> None:
        assert _resolve_expected_sha("z" * 40) is None


class TestCloudRunRequireSHA:
    """End-to-end smoke: verify the cloud-run command refuses drift.

    We patch everything that touches the network so the test stays
    deterministic. Success paths (SHA matches) aren't tested here
    because cloud_run then continues into workflow_dispatch, which
    is its own integration-test surface.
    """

    def test_mismatch_exits_nonzero(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from types import SimpleNamespace

        from click.testing import CliRunner

        from shipyard.cli import main

        expected = "a" * 40
        remote = "b" * 40

        def fake_run(cmd: list[str], *a: Any, **kw: Any) -> Any:
            class R:
                returncode = 0
                stdout = ""
                stderr = ""

            joined = " ".join(cmd) if isinstance(cmd, list) else str(cmd)
            if "rev-parse" in joined:
                R.stdout = expected + "\n"
            elif "repos/" in joined:
                R.stdout = remote + "\n"
            return R

        monkeypatch.setattr("shipyard.cli.subprocess.run", fake_run)
        monkeypatch.setattr(
            "shipyard.cli._git_branch", lambda: "feature/foo"
        )
        monkeypatch.setattr(
            "shipyard.cli._detect_repo_slug_or_empty",
            lambda: "origin-owner/origin-repo",
        )
        monkeypatch.setattr(
            "shipyard.cli.discover_workflows",
            lambda: {"build": object()},
        )
        monkeypatch.setattr(
            "shipyard.cli.default_workflow_key",
            lambda cfg, workflows: "build",
        )
        # SHA check now runs AFTER plan resolution (#54 P1) so the
        # dispatch-repo (not origin) is validated. Plan mock supplies
        # plan.repository + plan.ref as the comparison source.
        fake_plan = SimpleNamespace(
            repository="dispatch-owner/dispatch-repo",
            ref="feature/foo",
            workflow=SimpleNamespace(
                key="build", file="build.yml", name="Build"
            ),
            provider="github-hosted",
            dispatch_fields={},
            to_dict=lambda: {},
        )
        monkeypatch.setattr(
            "shipyard.cli.resolve_cloud_dispatch_plan",
            lambda **kw: fake_plan,
        )

        runner = CliRunner()
        result = runner.invoke(
            main,
            ["cloud", "run", "build", "--require-sha", "HEAD"],
        )
        assert result.exit_code == 1
        assert "Stale dispatch refused" in result.output
        assert "dispatch-owner/dispatch-repo" in result.output
