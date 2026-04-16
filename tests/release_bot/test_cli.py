"""CLI smoke tests for `shipyard release-bot`.

Exercises the command plumbing without hitting gh. We monkeypatch
`detect_state`, `set_secret`, and `verify_token` with fakes so the
interactive wizard can be driven by pytest's stdin.
"""

from __future__ import annotations

from datetime import datetime, timezone
from typing import Any

import pytest
from click.testing import CliRunner

from shipyard.cli import main
from shipyard.release_bot.setup import ReleaseBotError, ReleaseBotState


@pytest.fixture
def runner() -> CliRunner:
    return CliRunner()


def _patch(monkeypatch: pytest.MonkeyPatch, **overrides: Any) -> None:
    for name, value in overrides.items():
        monkeypatch.setattr(f"shipyard.cli.{name}", value)


class TestStatus:
    def test_missing_secret(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=False
            ),
        )
        result = runner.invoke(main, ["release-bot", "status"])
        assert result.exit_code == 0, result.output
        assert "missing" in result.output.lower()

    def test_auth_failure_prints_diagnosis(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug,
                secret_present=True,
                secret_updated_at=datetime(2026, 4, 1, tzinfo=timezone.utc),
                last_auto_release_conclusion="failure",
                last_auto_release_error_signature="auth",
            ),
        )
        result = runner.invoke(main, ["release-bot", "status"])
        assert result.exit_code == 0
        assert "rejected at actions/checkout" in result.output
        assert "release-bot setup --reconfigure" in result.output

    def test_json_mode(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug,
                secret_present=True,
                secret_updated_at=datetime(2026, 4, 1, tzinfo=timezone.utc),
            ),
        )
        result = runner.invoke(main, ["--json", "release-bot", "status"])
        assert result.exit_code == 0
        assert '"secret_present": true' in result.output

    def test_missing_repo_slug_exits_nonzero(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(monkeypatch, _detect_repo_slug_or_empty=lambda: "")
        result = runner.invoke(main, ["release-bot", "status"])
        assert result.exit_code == 1


class TestSetup:
    def test_already_configured_prompts_for_reconfigure(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=True
            ),
            set_secret=lambda *a, **kw: pytest.fail("should not set"),
            verify_token=lambda *a, **kw: pytest.fail("should not verify"),
        )
        result = runner.invoke(main, ["release-bot", "setup"])
        assert result.exit_code == 0
        assert "already set" in result.output.lower()

    def test_paste_flow_sets_secret(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        calls: dict[str, Any] = {}

        def fake_set(slug: str, token: str) -> None:
            calls["set"] = (slug, token)

        def fake_verify(slug: str, **kw: Any) -> str:
            calls["verify"] = slug
            return "success"

        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=False
            ),
            set_secret=fake_set,
            verify_token=fake_verify,
            open_browser=lambda url: False,
        )
        result = runner.invoke(
            main, ["release-bot", "setup", "--paste"], input="ghp_testvalue\n"
        )
        assert result.exit_code == 0, result.output
        assert calls["set"] == ("owner/repo", "ghp_testvalue")
        assert "actions/checkout accepted" in result.output

    def test_verify_failure_warns_but_exits_zero(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=False
            ),
            set_secret=lambda *a, **kw: None,
            verify_token=lambda *a, **kw: "failure",
            open_browser=lambda url: False,
        )
        result = runner.invoke(
            main, ["release-bot", "setup", "--paste"], input="some-token\n"
        )
        assert result.exit_code == 0
        assert "failure" in result.output
        assert "scope that" in result.output

    def test_verify_dispatch_error_reported(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def raising_verify(*a: Any, **kw: Any) -> str:
            raise ReleaseBotError("dispatch broke", "gh missing")

        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=False
            ),
            set_secret=lambda *a, **kw: None,
            verify_token=raising_verify,
            open_browser=lambda url: False,
        )
        result = runner.invoke(
            main, ["release-bot", "setup", "--paste"], input="tok\n"
        )
        assert result.exit_code == 0
        assert "dispatch broke" in result.output

    def test_no_verify_skips_dispatch(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=False
            ),
            set_secret=lambda *a, **kw: None,
            verify_token=lambda *a, **kw: pytest.fail("should not verify"),
            open_browser=lambda url: False,
        )
        result = runner.invoke(
            main,
            ["release-bot", "setup", "--paste", "--no-verify"],
            input="tok\n",
        )
        assert result.exit_code == 0
        assert "Stored RELEASE_BOT_TOKEN" in result.output

    def test_empty_token_aborts(
        self, runner: CliRunner, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        _patch(
            monkeypatch,
            _detect_repo_slug_or_empty=lambda: "owner/repo",
            detect_state=lambda slug, **kw: ReleaseBotState(
                repo_slug=slug, secret_present=False
            ),
            set_secret=lambda *a, **kw: pytest.fail("should not set"),
            open_browser=lambda url: False,
        )
        result = runner.invoke(
            main, ["release-bot", "setup", "--paste"], input="\n"
        )
        assert result.exit_code == 1
