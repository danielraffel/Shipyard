"""CLI-level tests for `shipyard ship --resume` and `ship-state` subcommands.

These test the resume-decision helpers directly (the heavy `ship`
command exercises too many subprocesses for a unit test; slice 6 of
the branch will add an end-to-end flow test).
"""

from __future__ import annotations

import tempfile
from datetime import datetime, timezone
from pathlib import Path

import pytest
from click.testing import CliRunner

from shipyard.cli import (
    _detect_ship_state_drift,
    _preview_ship_state_prune,
    _resolve_resume_mode,
    main,
)
from shipyard.core.ship_state import (
    DispatchedRun,
    ShipState,
    ShipStateStore,
    compute_policy_signature,
)


def _sample_state(pr: int = 224, sha: str = "abc1234def5") -> ShipState:
    return ShipState(
        pr=pr,
        repo="danielraffel/pulp",
        branch="feature/foo",
        base_branch="main",
        head_sha=sha,
        policy_signature=compute_policy_signature(
            ["macos", "linux"], ["mac", "ubuntu"], "FULL"
        ),
    )


class TestResolveResumeMode:
    def test_none_when_no_existing_state(self) -> None:
        assert _resolve_resume_mode(None, None) is None
        assert _resolve_resume_mode(True, None) is None
        assert _resolve_resume_mode(False, None) is None

    def test_default_resumes_when_state_exists(self) -> None:
        assert _resolve_resume_mode(None, _sample_state()) is True

    def test_explicit_no_resume_overrides(self) -> None:
        assert _resolve_resume_mode(False, _sample_state()) is False

    def test_explicit_resume_passes_through(self) -> None:
        assert _resolve_resume_mode(True, _sample_state()) is True


class TestDetectDrift:
    def test_no_drift(self) -> None:
        s = _sample_state()
        assert _detect_ship_state_drift(
            s, current_sha=s.head_sha, current_policy=s.policy_signature
        ) is None

    def test_sha_drift(self) -> None:
        s = _sample_state(sha="abc1234def5")
        msg = _detect_ship_state_drift(
            s, current_sha="ffffffffffff", current_policy=s.policy_signature
        )
        assert msg is not None
        assert "SHA" in msg or "sha" in msg.lower()

    def test_policy_drift(self) -> None:
        s = _sample_state()
        msg = _detect_ship_state_drift(
            s, current_sha=s.head_sha, current_policy="otherpolicy"
        )
        assert msg is not None
        assert "policy" in msg.lower()

    def test_policy_drift_ignored_when_signature_empty(self) -> None:
        # Legacy / freshly-created states may have an empty signature;
        # don't refuse to resume based on empty-vs-nonempty alone.
        s = _sample_state()
        s.policy_signature = ""
        assert _detect_ship_state_drift(
            s, current_sha=s.head_sha, current_policy="anything"
        ) is None


class TestPreviewPrune:
    def test_preview_reports_aged_archive(self, tmp_path: Path) -> None:
        store = ShipStateStore(path=tmp_path / "ship")
        store.save(_sample_state(pr=1))
        archived = store.archive(1)
        assert archived is not None
        # Backdate the archive file to look aged.
        import os

        from datetime import timedelta
        old = (datetime.now(timezone.utc) - timedelta(days=60)).timestamp()
        os.utime(archived, (old, old))
        preview = _preview_ship_state_prune(store)
        assert preview["total"] == 1
        assert archived.name in preview["deleted_archived"]
        # Active file pruning is explicitly skipped in dry-run.
        assert preview["deleted_active"] == []


class TestDetectRepoSlug:
    def test_returns_slug_from_reporef(self, monkeypatch: pytest.MonkeyPatch) -> None:
        from dataclasses import dataclass

        from shipyard.cli import _detect_repo_slug_or_empty

        @dataclass
        class _Ref:
            @property
            def slug(self) -> str:
                return "owner/repo"

        monkeypatch.setattr(
            "shipyard.cli.detect_repo_from_remote", lambda: _Ref()
        )
        assert _detect_repo_slug_or_empty() == "owner/repo"

    def test_empty_on_none(self, monkeypatch: pytest.MonkeyPatch) -> None:
        from shipyard.cli import _detect_repo_slug_or_empty

        monkeypatch.setattr(
            "shipyard.cli.detect_repo_from_remote", lambda: None
        )
        assert _detect_repo_slug_or_empty() == ""

    def test_empty_on_exception(self, monkeypatch: pytest.MonkeyPatch) -> None:
        from shipyard.cli import _detect_repo_slug_or_empty

        def _raise() -> None:
            raise RuntimeError("git missing")

        monkeypatch.setattr(
            "shipyard.cli.detect_repo_from_remote", _raise
        )
        assert _detect_repo_slug_or_empty() == ""


class TestShipStateCommandSmoke:
    """Smoke-test the ship-state subcommand group with a pinned state dir.

    These tests pin `Context.ship_state` to a tmp-path store so they
    never read or write the user's real Shipyard state dir (which
    differs per-OS and does not honor XDG on macOS).
    """

    def _runner_with_store(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> CliRunner:
        store = ShipStateStore(path=tmp_path / "ship")
        monkeypatch.setattr(
            "shipyard.cli.Context.ship_state",
            property(lambda self: store),
        )
        return CliRunner()

    def test_list_empty(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner = self._runner_with_store(tmp_path, monkeypatch)
        result = runner.invoke(main, ["ship-state", "list"])
        assert result.exit_code == 0, result.output
        assert "No active ship state." in result.output

    def test_show_missing_fails(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        runner = self._runner_with_store(tmp_path, monkeypatch)
        result = runner.invoke(main, ["ship-state", "show", "4242"])
        assert result.exit_code == 1
