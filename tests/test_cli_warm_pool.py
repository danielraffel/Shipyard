"""Integration tests for warm-pool reuse in the run / targets CLI.

Verifies the end-to-end gate in ``_execute_job``: a first PASS
records an entry, a second ship on the same SHA re-enters that
workdir with ``resume_from=configure``, and a FAIL evicts the
entry. Also covers the three disable levels (per-target,
``SHIPYARD_NO_WARM_POOL`` env, ``--no-warm`` CLI flag), the GitHub-
hosted ineligibility warning, and the ``targets warm status|drain``
subcommands.
"""

from __future__ import annotations

import json
from datetime import datetime, timezone
from typing import TYPE_CHECKING

from click.testing import CliRunner

from shipyard.cli import main
from shipyard.core.config import Config
from shipyard.core.job import TargetResult, TargetStatus
from shipyard.preflight import PreflightResult
from shipyard.targets.warm_pool import WarmPool, default_pool_path

if TYPE_CHECKING:
    from pathlib import Path


def _config(tmp_path: Path, targets: dict) -> Config:
    project_dir = tmp_path / ".shipyard"
    project_dir.mkdir()
    return Config(
        data={
            "project": {"name": "shipyard", "platforms": ["linux"]},
            "validation": {"default": {"command": "pytest"}},
            "targets": targets,
        },
        project_dir=project_dir,
    )


def _preflight(config: Config) -> PreflightResult:
    return PreflightResult(
        git_root=config.project_dir.parent if config.project_dir else None,
        expected_root=config.project_dir.parent if config.project_dir else None,
        targets={},
        warnings=[],
    )


def _install_common_stubs(monkeypatch, config: Config, state_dir: Path) -> None:
    monkeypatch.setattr("shipyard.cli.Config.load_from_cwd", lambda cwd=None: config)
    monkeypatch.setattr("shipyard.core.config._default_state_dir", lambda: state_dir)
    monkeypatch.setattr("shipyard.cli._git_sha", lambda: "a" * 40)
    monkeypatch.setattr("shipyard.cli._git_branch", lambda: "feature/warm")
    monkeypatch.setattr(
        "shipyard.cli.run_submission_preflight",
        lambda *args, **kwargs: _preflight(config),
    )


def _pass_result() -> TargetResult:
    now = datetime.now(timezone.utc)
    return TargetResult(
        target_name="ubuntu",
        platform="linux-x64",
        status=TargetStatus.PASS,
        backend="ssh",
        started_at=now,
        completed_at=now,
    )


def _fail_result() -> TargetResult:
    now = datetime.now(timezone.utc)
    return TargetResult(
        target_name="ubuntu",
        platform="linux-x64",
        status=TargetStatus.FAIL,
        backend="ssh",
        started_at=now,
        completed_at=now,
        error_message="boom",
    )


class TestWarmPoolReuse:
    def test_first_pass_records_entry(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(
            tmp_path,
            {
                "ubuntu": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ubuntu",
                    "warm_keepalive_seconds": 600,
                    "repo_path": "~/repo",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)

        monkeypatch.setattr(
            "shipyard.executor.ssh.SSHExecutor.validate",
            lambda self, **kwargs: _pass_result(),
        )

        runner = CliRunner()
        result = runner.invoke(main, ["--json", "run", "--targets", "ubuntu"])
        assert result.exit_code == 0, result.output

        pool = WarmPool(default_pool_path(state_dir))
        entries = pool.all_entries()
        assert len(entries) == 1
        assert entries[0].target == "ubuntu"
        assert entries[0].host == "ubuntu"
        assert entries[0].sha == "a" * 40
        assert entries[0].workdir == "~/repo"

    def test_second_run_reuses_with_resume_from_configure(
        self, tmp_path, monkeypatch,
    ) -> None:
        state_dir = tmp_path / "state"
        config = _config(
            tmp_path,
            {
                "ubuntu": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ubuntu",
                    "warm_keepalive_seconds": 600,
                    "repo_path": "~/original",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)

        # Seed a pool entry as if a prior PASS landed.
        from shipyard.targets.warm_pool import PoolEntry, compute_expires_at
        now = datetime.now(timezone.utc).timestamp()
        pool = WarmPool(default_pool_path(state_dir))
        pool.upsert(PoolEntry(
            target="ubuntu",
            host="ubuntu",
            backend="ssh",
            workdir="/srv/warm-workdir",
            sha="a" * 40,
            expires_at=compute_expires_at(600, now=now),
            created_at=now,
        ))

        captured: dict[str, object] = {}

        def fake_validate(self, **kwargs):
            captured.update(kwargs)
            return _pass_result()

        monkeypatch.setattr(
            "shipyard.executor.ssh.SSHExecutor.validate", fake_validate,
        )

        runner = CliRunner()
        result = runner.invoke(main, ["--json", "run", "--targets", "ubuntu"])
        assert result.exit_code == 0, result.output

        # The warm workdir overrode the config default and the
        # dispatcher received resume_from="configure" so pre-stage
        # was skipped.
        assert captured["resume_from"] == "configure"
        assert captured["target_config"]["repo_path"] == "/srv/warm-workdir"

    def test_failure_during_warm_reuse_evicts(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(
            tmp_path,
            {
                "ubuntu": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ubuntu",
                    "warm_keepalive_seconds": 600,
                    "repo_path": "~/repo",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)

        from shipyard.targets.warm_pool import PoolEntry, compute_expires_at
        now = datetime.now(timezone.utc).timestamp()
        pool = WarmPool(default_pool_path(state_dir))
        pool.upsert(PoolEntry(
            target="ubuntu",
            host="ubuntu",
            backend="ssh",
            workdir="/srv/warm-workdir",
            sha="a" * 40,
            expires_at=compute_expires_at(600, now=now),
            created_at=now,
        ))

        monkeypatch.setattr(
            "shipyard.executor.ssh.SSHExecutor.validate",
            lambda self, **kwargs: _fail_result(),
        )

        runner = CliRunner()
        runner.invoke(main, ["--json", "run", "--targets", "ubuntu"])

        pool_after = WarmPool(default_pool_path(state_dir))
        assert pool_after.all_entries() == []

    def test_no_warm_cli_flag_skips_reuse(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(
            tmp_path,
            {
                "ubuntu": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ubuntu",
                    "warm_keepalive_seconds": 600,
                    "repo_path": "~/original",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)

        from shipyard.targets.warm_pool import PoolEntry, compute_expires_at
        now = datetime.now(timezone.utc).timestamp()
        pool = WarmPool(default_pool_path(state_dir))
        pool.upsert(PoolEntry(
            target="ubuntu",
            host="ubuntu",
            backend="ssh",
            workdir="/srv/warm-workdir",
            sha="a" * 40,
            expires_at=compute_expires_at(600, now=now),
            created_at=now,
        ))

        captured: dict[str, object] = {}

        def fake_validate(self, **kwargs):
            captured.update(kwargs)
            return _pass_result()

        monkeypatch.setattr(
            "shipyard.executor.ssh.SSHExecutor.validate", fake_validate,
        )

        runner = CliRunner()
        # --no-warm forces cold-start regardless of pool contents.
        result = runner.invoke(
            main, ["--json", "run", "--targets", "ubuntu", "--no-warm"],
        )
        assert result.exit_code == 0, result.output

        # resume_from is None (no reuse) and repo_path wasn't
        # overridden by the warm workdir.
        assert captured["resume_from"] is None
        assert captured["target_config"]["repo_path"] == "~/original"

    def test_env_kill_switch_skips_reuse(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(
            tmp_path,
            {
                "ubuntu": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ubuntu",
                    "warm_keepalive_seconds": 600,
                    "repo_path": "~/original",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)
        monkeypatch.setenv("SHIPYARD_NO_WARM_POOL", "1")

        from shipyard.targets.warm_pool import PoolEntry, compute_expires_at
        now = datetime.now(timezone.utc).timestamp()
        pool = WarmPool(default_pool_path(state_dir))
        pool.upsert(PoolEntry(
            target="ubuntu",
            host="ubuntu",
            backend="ssh",
            workdir="/srv/warm-workdir",
            sha="a" * 40,
            expires_at=compute_expires_at(600, now=now),
            created_at=now,
        ))

        captured: dict[str, object] = {}

        def fake_validate(self, **kwargs):
            captured.update(kwargs)
            return _pass_result()

        monkeypatch.setattr(
            "shipyard.executor.ssh.SSHExecutor.validate", fake_validate,
        )

        runner = CliRunner()
        result = runner.invoke(main, ["--json", "run", "--targets", "ubuntu"])
        assert result.exit_code == 0, result.output
        assert captured["resume_from"] is None
        assert captured["target_config"]["repo_path"] == "~/original"

    def test_keepalive_zero_is_off_per_target(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(
            tmp_path,
            {
                "ubuntu": {
                    "backend": "ssh",
                    "platform": "linux-x64",
                    "host": "ubuntu",
                    "warm_keepalive_seconds": 0,
                    "repo_path": "~/repo",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)

        monkeypatch.setattr(
            "shipyard.executor.ssh.SSHExecutor.validate",
            lambda self, **kwargs: _pass_result(),
        )

        runner = CliRunner()
        result = runner.invoke(main, ["--json", "run", "--targets", "ubuntu"])
        assert result.exit_code == 0, result.output

        # PASS on a keepalive=0 target does NOT write a pool entry.
        pool = WarmPool(default_pool_path(state_dir))
        assert pool.all_entries() == []

    def test_github_hosted_backend_ignored_with_warn(
        self, tmp_path, monkeypatch,
    ) -> None:
        state_dir = tmp_path / "state"
        # Cloud backend — workflow runs are ephemeral so warm
        # reuse is a no-op. The warning goes to human output, not
        # JSON — exercise the human path here.
        config = _config(
            tmp_path,
            {
                "win": {
                    "backend": "cloud",
                    "platform": "windows-x64",
                    "warm_keepalive_seconds": 600,
                    "workflow": "ci.yml",
                    "repository": "acme/x",
                }
            },
        )
        _install_common_stubs(monkeypatch, config, state_dir)

        def fake_validate(self, **kwargs):
            return TargetResult(
                target_name="win",
                platform="windows-x64",
                status=TargetStatus.PASS,
                backend="cloud",
                started_at=datetime.now(timezone.utc),
                completed_at=datetime.now(timezone.utc),
            )

        monkeypatch.setattr(
            "shipyard.executor.cloud.CloudExecutor.validate", fake_validate,
        )
        monkeypatch.setattr(
            "shipyard.executor.cloud.CloudExecutor.probe",
            lambda self, target_config: True,
        )

        runner = CliRunner()
        result = runner.invoke(main, ["run", "--targets", "win"])
        assert result.exit_code == 0, result.output
        assert "warm_keepalive_seconds" in result.output
        assert "ephemeral" in result.output

        # No pool entry written for an ineligible backend.
        pool = WarmPool(default_pool_path(state_dir))
        assert pool.all_entries() == []


class TestWarmPoolSubcommands:
    def test_status_empty(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(tmp_path, {})
        _install_common_stubs(monkeypatch, config, state_dir)

        runner = CliRunner()
        result = runner.invoke(main, ["--json", "targets", "warm", "status"])
        assert result.exit_code == 0, result.output
        assert json.loads(result.output)["entries"] == []

    def test_status_lists_entries(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(tmp_path, {})
        _install_common_stubs(monkeypatch, config, state_dir)

        from shipyard.targets.warm_pool import PoolEntry, compute_expires_at
        now = datetime.now(timezone.utc).timestamp()
        pool = WarmPool(default_pool_path(state_dir))
        pool.upsert(PoolEntry(
            target="ubuntu",
            host="ubuntu",
            backend="ssh",
            workdir="~/repo",
            sha="b" * 40,
            expires_at=compute_expires_at(600, now=now),
            created_at=now,
        ))

        runner = CliRunner()
        result = runner.invoke(main, ["--json", "targets", "warm", "status"])
        assert result.exit_code == 0, result.output
        entries = json.loads(result.output)["entries"]
        assert len(entries) == 1
        assert entries[0]["target"] == "ubuntu"
        assert entries[0]["sha"] == "b" * 40
        assert entries[0]["ttl_remaining_secs"] > 0

    def test_drain_removes_all(self, tmp_path, monkeypatch) -> None:
        state_dir = tmp_path / "state"
        config = _config(tmp_path, {})
        _install_common_stubs(monkeypatch, config, state_dir)

        from shipyard.targets.warm_pool import PoolEntry, compute_expires_at
        now = datetime.now(timezone.utc).timestamp()
        pool = WarmPool(default_pool_path(state_dir))
        pool.upsert(PoolEntry(
            target="ubuntu",
            host="ubuntu",
            backend="ssh",
            workdir="~/repo",
            sha="b" * 40,
            expires_at=compute_expires_at(600, now=now),
            created_at=now,
        ))

        runner = CliRunner()
        result = runner.invoke(
            main, ["--json", "targets", "warm", "drain", "--yes"],
        )
        assert result.exit_code == 0, result.output
        payload = json.loads(result.output)
        assert payload["drained"] == 1

        pool_after = WarmPool(default_pool_path(state_dir))
        assert pool_after.all_entries() == []
