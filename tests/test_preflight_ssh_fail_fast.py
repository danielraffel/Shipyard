"""Tests for SSH fail-fast preflight behavior (#100)."""

from __future__ import annotations

import subprocess
import time
from pathlib import Path
from typing import Any

import pytest

from shipyard.core.config import Config
from shipyard.executor.dispatch import ExecutorDispatcher
from shipyard.executor.ssh import SSHExecutor, _classify_probe_error
from shipyard.preflight import BackendUnreachableError, run_submission_preflight


def _config(tmp_path: Path, targets: dict[str, dict[str, Any]]) -> Config:
    project_dir = tmp_path / ".shipyard"
    project_dir.mkdir()
    return Config(
        data={"project": {"name": "test"}, "targets": targets},
        project_dir=project_dir,
    )


class TestSSHClassifier:
    """The classifier's buckets are load-bearing for the CLI error output."""

    @pytest.mark.parametrize(
        "stderr,expected",
        [
            ("Permission denied (publickey).", "auth"),
            ("Too many authentication failures", "auth"),
            ("Host key verification failed.", "host_key"),
            ("REMOTE HOST IDENTIFICATION HAS CHANGED!", "host_key"),
            ("ssh: Could not resolve hostname bogus: Name or service not known", "network"),
            ("ssh: Could not resolve hostname bogus: nodename nor servname provided", "network"),
            ("ssh: connect to host bogus port 22: No route to host", "network"),
            ("ssh: connect to host 10.0.0.1 port 22: Connection refused", "network"),
            ("ssh: connect to host 10.0.0.1 port 22: Connection timed out", "timeout"),
            ("some weird unrelated output", "unknown"),
            ("", "network"),  # rc=255 fallback
        ],
    )
    def test_stable_categories(self, stderr: str, expected: str) -> None:
        rc = 255 if not stderr else 1
        assert _classify_probe_error(stderr, rc) == expected


class TestSSHProbeDiagnosis:
    """SSHExecutor.diagnose() surfaces rich reachability context."""

    def test_probe_honors_10s_budget(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """A hung ssh probe must surface within ~10s, not the full cmd timeout."""
        calls: list[int] = []

        def fake_run(cmd: list[str], **kw: Any) -> subprocess.CompletedProcess:
            calls.append(kw.get("timeout", 0))
            raise subprocess.TimeoutExpired(cmd=cmd, timeout=kw.get("timeout", 0))

        monkeypatch.setattr(subprocess, "run", fake_run)
        start = time.monotonic()
        diag = SSHExecutor().diagnose({"host": "bogus.invalid"})
        assert time.monotonic() - start < 2.0  # fake_run raises immediately
        assert diag["reachable"] is False
        assert diag["category"] == "timeout"
        assert calls == [10], (
            "probe must pass a 10s timeout to subprocess.run, "
            "not the validation timeout"
        )

    def test_missing_host_reports_configuration(self) -> None:
        diag = SSHExecutor().diagnose({})  # no host
        assert diag["reachable"] is False
        assert diag["category"] == "configuration"
        assert "no host configured" in diag["message"].lower()

    def test_auth_refused_is_classified(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def fake_run(*args: Any, **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=[], returncode=255, stdout="",
                stderr="Permission denied (publickey).\r\n",
            )

        monkeypatch.setattr(subprocess, "run", fake_run)
        diag = SSHExecutor().diagnose({"host": "user@host"})
        assert diag["reachable"] is False
        assert diag["category"] == "auth"
        assert "permission denied" in diag["message"].lower()
        assert "host" in diag["message"]

    def test_reachable_host_round_trips(
        self, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        def fake_run(*args: Any, **kw: Any) -> subprocess.CompletedProcess:
            return subprocess.CompletedProcess(
                args=[], returncode=0, stdout="ok\n", stderr="",
            )

        monkeypatch.setattr(subprocess, "run", fake_run)
        diag = SSHExecutor().diagnose({"host": "ubuntu"})
        assert diag["reachable"] is True
        assert diag["category"] is None


class TestPreflightSkipAndAllowSemantics:
    """--skip-target and --allow-unreachable-targets differ by design."""

    def test_skip_target_bypasses_probe_entirely(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        config = _config(
            tmp_path,
            {
                "ubuntu": {"backend": "ssh", "platform": "linux-x64", "host": "u"},
                "mac": {"backend": "local", "platform": "macos-arm64"},
            },
        )
        dispatcher = ExecutorDispatcher()
        monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)

        probe_calls: list[dict[str, Any]] = []

        def fake_probe(target_config: dict[str, Any]) -> bool:
            probe_calls.append(target_config)
            return True

        monkeypatch.setattr(dispatcher, "probe", fake_probe)

        result = run_submission_preflight(
            config,
            target_names=["ubuntu", "mac"],
            dispatcher=dispatcher,
            skip_targets=["ubuntu"],
            cwd=tmp_path,
        )
        # Skipped target was excluded before the probe — no probe call.
        assert [c.get("name") for c in probe_calls] == ["mac"]
        assert result.skipped_targets == ["ubuntu"]
        assert any("deliberately skipped" in w for w in result.warnings)

    def test_skip_target_rejects_unknown_name(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        config = _config(
            tmp_path,
            {"mac": {"backend": "local", "platform": "macos-arm64"}},
        )
        dispatcher = ExecutorDispatcher()
        monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)

        with pytest.raises(ValueError, match="no configured target"):
            run_submission_preflight(
                config,
                target_names=["mac"],
                dispatcher=dispatcher,
                skip_targets=["windows"],  # not configured
                cwd=tmp_path,
            )

    def test_allow_unreachable_emits_loud_validation_gap_warning(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        config = _config(
            tmp_path,
            {"ubuntu": {"backend": "ssh", "platform": "linux-x64", "host": "u"}},
        )
        dispatcher = ExecutorDispatcher()
        monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)
        monkeypatch.setattr(dispatcher, "probe", lambda _c: False)

        result = run_submission_preflight(
            config,
            target_names=["ubuntu"],
            dispatcher=dispatcher,
            allow_unreachable_targets=True,
            cwd=tmp_path,
        )

        # The warning has to be impossible to miss — the whole reason
        # this flag exists is that consumers were using it as muscle
        # memory when a real backend outage hit.
        joined = "\n".join(result.warnings)
        assert "VALIDATION GAP" in joined
        assert "SKIPPED, NOT validated" in joined
        assert "ubuntu" in joined

    def test_backend_unreachable_error_carries_rich_detail(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        config = _config(
            tmp_path,
            {"ubuntu": {"backend": "ssh", "platform": "linux-x64", "host": "u"}},
        )
        dispatcher = ExecutorDispatcher()
        monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)
        monkeypatch.setattr(dispatcher, "probe", lambda _c: False)
        monkeypatch.setattr(
            dispatcher,
            "diagnose",
            lambda _c: {
                "reachable": False,
                "message": "SSH backend unreachable at u.\n  Failure category: auth",
                "category": "auth",
            },
        )

        with pytest.raises(BackendUnreachableError) as exc:
            run_submission_preflight(
                config,
                target_names=["ubuntu"],
                dispatcher=dispatcher,
                cwd=tmp_path,
            )
        msg = str(exc.value)
        # Names the target
        assert "'ubuntu'" in msg
        # Includes the rich diagnosis from the dispatcher
        assert "category: auth" in msg.lower()
        # Explains the three escape hatches with their semantics
        assert "--skip-target" in msg
        assert "--allow-unreachable-targets" in msg
        assert "LANE WILL BE SKIPPED, NOT VALIDATED" in msg


class TestIntegrationWithUnresolvableHost:
    """Acceptance: unresolvable hostname surfaces within 10s."""

    @pytest.mark.integration
    def test_real_ssh_probe_fails_fast_on_unresolvable_host(
        self, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
    ) -> None:
        """Real ssh binary hit, no mocking. Must return within ~15s."""
        config = _config(
            tmp_path,
            {
                "ghost": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ghost-backend-that-does-not-exist.invalid",
                }
            },
        )
        dispatcher = ExecutorDispatcher()
        monkeypatch.setattr("shipyard.preflight._git_root_for", lambda _: tmp_path)

        start = time.monotonic()
        with pytest.raises(BackendUnreachableError) as exc:
            run_submission_preflight(
                config,
                target_names=["ghost"],
                dispatcher=dispatcher,
                cwd=tmp_path,
            )
        elapsed = time.monotonic() - start

        assert elapsed < 15.0, f"probe took {elapsed:.1f}s (budget 10s + margin)"
        assert "ghost" in str(exc.value)
