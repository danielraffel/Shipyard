"""Smoke tests for `shipyard doctor --release-chain`."""

from __future__ import annotations

from typing import Any

import pytest
from click.testing import CliRunner

from shipyard.cli import main
from shipyard.release_bot.setup import ReleaseBotError


def _patch(monkeypatch: pytest.MonkeyPatch, **overrides: Any) -> None:
    for name, value in overrides.items():
        monkeypatch.setattr(f"shipyard.cli.{name}", value)


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


class TestDoctorReleaseChain:
    def test_flag_off_does_not_dispatch(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            verify_token=lambda *a, **kw: pytest.fail("should not dispatch"),
        )
        # Don't care about the rest of doctor's sections; just ensure
        # the verify_token path isn't triggered without the flag.
        result = runner.invoke(main, ["doctor"])
        # Exit code reflects core-tool health; test only cares that
        # verify_token wasn't called (covered by the pytest.fail above).
        assert result.exit_code in (0, 1)

    def test_flag_on_reports_success(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            verify_token=lambda slug, **kw: "success",
        )
        result = runner.invoke(main, ["--json", "doctor", "--release-chain"])
        assert result.exit_code in (0, 1)
        assert "checkout-ok" in result.output

    def test_flag_on_reports_dispatch_failure(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def raise_(*a: Any, **kw: Any) -> str:
            raise ReleaseBotError("dispatch bad", "detail here")

        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            verify_token=raise_,
        )
        result = runner.invoke(main, ["--json", "doctor", "--release-chain"])
        assert result.exit_code in (0, 1)
        assert "dispatch-failed" in result.output
        assert "dispatch bad" in result.output

    def test_flag_on_reports_workflow_failure(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            verify_token=lambda slug, **kw: "failure",
        )
        result = runner.invoke(main, ["--json", "doctor", "--release-chain"])
        assert result.exit_code in (0, 1)
        assert "failure" in result.output
        assert "release-bot" in result.output

    def test_no_repo_slug_skips_silently(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "",
            verify_token=lambda *a, **kw: pytest.fail("should not dispatch"),
        )
        result = runner.invoke(main, ["doctor", "--release-chain"])
        assert result.exit_code in (0, 1)

    def test_workflow_success_but_secret_missing_reports_fallback(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Reproduces #52 P2: auto-release.yml's `secrets.X || GITHUB_TOKEN`
        # fallback means a "success" conclusion does NOT prove the PAT
        # works. When the secret is absent we must say so.
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            _check_release_bot_token=lambda: {
                "RELEASE_BOT_TOKEN": {"ok": False, "version": "missing"}
            },
            verify_token=lambda slug, **kw: "success",
        )
        result = runner.invoke(main, ["--json", "doctor", "--release-chain"])
        assert result.exit_code in (0, 1)
        assert "fallback-token" in result.output
        assert "GITHUB_TOKEN fallback" in result.output

    def test_workflow_success_with_unknown_secret_state(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Reproduces #55 P2 + #56 P2: when _check_release_bot_token
        # returns None (secret listing unreadable — auth/scope),
        # we must NOT cry "fallback-token" and we must surface the
        # uncertainty in `version` (not only `detail`) so it shows
        # up in human doctor output where render_doctor() prefers
        # `version` to `detail`.
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            _check_release_bot_token=lambda: None,
            verify_token=lambda slug, **kw: "success",
        )
        result = runner.invoke(main, ["--json", "doctor", "--release-chain"])
        assert result.exit_code in (0, 1)
        # `version` is the human-visible field.
        assert "checkout-ok-unverified" in result.output
        assert "fallback-token" not in result.output
        # And ok=False because we didn't verify the PAT specifically.
        assert '"ok": false' in result.output
