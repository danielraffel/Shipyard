"""Tests for release_bot.setup — pure-logic functions only.

Side-effecting helpers (detect_state, set_secret, verify_token)
are exercised in integration tests, not here.
"""

from __future__ import annotations

import urllib.parse
from datetime import datetime, timezone
from typing import TYPE_CHECKING

from shipyard.release_bot.setup import (
    ReleaseBotError,
    ReleaseBotState,
    describe_state,
    plan_setup,
    render_pat_creation_url,
)

if TYPE_CHECKING:
    import pytest


def _state(**overrides):
    base = {
        "repo_slug": "danielraffel/my-app",
        "secret_present": False,
    }
    base.update(overrides)
    return ReleaseBotState(**base)


class TestPlanSetup:
    def test_fresh_project_recommends_create_new(self) -> None:
        plan = plan_setup(_state())
        assert plan.recommended == "create-new"
        assert plan.suggested_pat_name == "my-app-release-bot"
        assert "least-privilege" in plan.reasoning.lower()

    def test_existing_sibling_secret_recommends_expand(self) -> None:
        s = _state(other_repos_with_secret=["danielraffel/pulp"])
        plan = plan_setup(s)
        assert plan.recommended == "expand-existing"
        assert "pulp" in plan.reasoning

    def test_shared_name_forces_create_new_with_custom_name(self) -> None:
        plan = plan_setup(_state(), shared_name="shipyard-release-bot")
        assert plan.recommended == "create-new"
        assert plan.suggested_pat_name == "shipyard-release-bot"
        assert "shipyard-release-bot" in plan.reasoning

    def test_suggested_name_strips_owner_prefix(self) -> None:
        plan = plan_setup(_state(repo_slug="SomeOwner/My-Project"))
        assert plan.suggested_pat_name == "my-project-release-bot"


class TestDescribeState:
    def test_missing_secret(self) -> None:
        lines = describe_state(_state())
        assert "repo: danielraffel/my-app" in lines
        assert any("missing" in line for line in lines)

    def test_configured_secret_with_timestamp(self) -> None:
        s = _state(
            secret_present=True,
            secret_updated_at=datetime(2026, 4, 15, tzinfo=timezone.utc),
        )
        lines = describe_state(s)
        assert any("configured" in line and "2026-04-15" in line for line in lines)

    def test_auth_error_surfaces_diagnosis_hint(self) -> None:
        s = _state(
            secret_present=True,
            last_auto_release_conclusion="failure",
            last_auto_release_error_signature="auth",
        )
        joined = "\n".join(describe_state(s))
        assert "rejected at actions/checkout" in joined

    def test_other_repos_listed(self) -> None:
        s = _state(other_repos_with_secret=["danielraffel/pulp", "a/b"])
        lines = describe_state(s)
        assert any("danielraffel/pulp" in line and "a/b" in line for line in lines)

    def test_no_auth_error_without_failure_signature(self) -> None:
        s = _state(
            secret_present=True,
            last_auto_release_conclusion="success",
        )
        joined = "\n".join(describe_state(s))
        assert "rejected" not in joined


class TestPatCreationURL:
    def test_url_shape(self) -> None:
        url = render_pat_creation_url(
            owner="danielraffel", pat_name="my-app-release-bot", repo="my-app"
        )
        parsed = urllib.parse.urlparse(url)
        assert parsed.scheme == "https"
        assert parsed.netloc == "github.com"
        assert parsed.path == "/settings/personal-access-tokens/new"

        q = urllib.parse.parse_qs(parsed.query)
        assert q["type"] == ["beta"]
        assert q["name"] == ["my-app-release-bot"]
        assert q["target_name"] == ["danielraffel"]
        assert "Shipyard release bot" in q["description"][0]

    def test_expiration_configurable(self) -> None:
        url = render_pat_creation_url(
            owner="o", pat_name="n", repo="r", expiration_days=30
        )
        q = urllib.parse.parse_qs(urllib.parse.urlparse(url).query)
        assert q["expires_in"] == ["30"]


class TestReleaseBotError:
    def test_message_and_detail_stored(self) -> None:
        e = ReleaseBotError("main message", "long detail")
        assert e.message == "main message"
        assert e.detail == "long detail"
        assert str(e) == "main message"

    def test_detail_defaults_empty(self) -> None:
        e = ReleaseBotError("only msg")
        assert e.detail == ""


class TestVerifyTokenRunIdCorrelation:
    """Regression: #51 P1 → #55 P1 → #56 P1.

    Cycling through timestamp-based novelty checks kept exposing
    subtler aliasing bugs (microseconds, same-second collisions).
    Final fix: correlate on databaseId — monotonically increasing —
    and require the polled run's ID to be strictly greater than a
    pre-dispatch baseline. These tests assert that invariant.
    """

    def _stub_dispatch(self, monkeypatch: pytest.MonkeyPatch) -> None:
        from shipyard.release_bot import setup as sut

        def fake_run(cmd, *a, **kw):
            class R:
                returncode = 0
                stderr = ""
                stdout = ""
            return R

        monkeypatch.setattr(sut.subprocess, "run", fake_run)
        monkeypatch.setattr(sut, "_default_branch", lambda slug: "main")

    def test_stale_same_second_run_is_ignored(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Baseline exists (ID 100). Subsequent poll still shows ID 100
        # (new run not surfaced yet). verify_token must NOT return —
        # it should keep polling until a strictly-higher ID appears.
        from shipyard.release_bot import setup as sut

        self._stub_dispatch(monkeypatch)

        call_count = {"n": 0}

        def fake_last_run(slug, workflow):
            call_count["n"] += 1
            if call_count["n"] == 1:
                # Pre-dispatch baseline call.
                return {"databaseId": 100, "status": "completed",
                        "conclusion": "success"}
            if call_count["n"] <= 3:
                # First two polls still show the stale run.
                return {"databaseId": 100, "status": "completed",
                        "conclusion": "failure"}
            # New run finally surfaces.
            return {"databaseId": 101, "status": "completed",
                    "conclusion": "success"}

        monkeypatch.setattr(sut, "_last_workflow_run", fake_last_run)
        import time as real_time
        monkeypatch.setattr(real_time, "sleep", lambda s: None)

        conclusion = sut.verify_token("owner/repo")
        assert conclusion == "success"
        # Baseline + stale-stall (×2) + fresh, at minimum.
        assert call_count["n"] >= 4

    def test_fresh_run_with_higher_id_accepted(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        from shipyard.release_bot import setup as sut

        self._stub_dispatch(monkeypatch)

        call_count = {"n": 0}

        def fake_last_run(slug, workflow):
            call_count["n"] += 1
            if call_count["n"] == 1:
                return {"databaseId": 50, "status": "completed",
                        "conclusion": "failure"}
            return {"databaseId": 51, "status": "completed",
                    "conclusion": "success"}

        monkeypatch.setattr(sut, "_last_workflow_run", fake_last_run)
        import time as real_time
        monkeypatch.setattr(real_time, "sleep", lambda s: None)

        assert sut.verify_token("owner/repo") == "success"

    def test_no_prior_run_accepts_first_completed(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        # Fresh repo — no baseline. verify_token should accept the
        # first completed run it sees (any valid ID is > None baseline).
        from shipyard.release_bot import setup as sut

        self._stub_dispatch(monkeypatch)

        call_count = {"n": 0}

        def fake_last_run(slug, workflow):
            call_count["n"] += 1
            if call_count["n"] == 1:
                return None  # baseline: no prior run exists
            return {"databaseId": 1, "status": "completed",
                    "conclusion": "success"}

        monkeypatch.setattr(sut, "_last_workflow_run", fake_last_run)
        import time as real_time
        monkeypatch.setattr(real_time, "sleep", lambda s: None)

        assert sut.verify_token("owner/repo") == "success"
