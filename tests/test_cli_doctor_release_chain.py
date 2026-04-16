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
