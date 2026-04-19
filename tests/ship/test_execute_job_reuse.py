"""Integration tests for cross-PR reuse in ``_execute_job``.

These drive the real ``_execute_job`` loop with a fake dispatcher so
we can assert:
  * a target with reuse globs whose diff misses the globs is **not**
    dispatched (fake dispatcher's counter stays at 0) and ends up
    with a synthetic PASS ``TargetResult`` whose ``reused_from`` is
    set.
  * a target without reuse globs is dispatched normally.
  * the recorded ``EvidenceRecord`` carries ``reused_from``.
"""

from __future__ import annotations

import subprocess
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import TYPE_CHECKING

import pytest

from shipyard.cli import Context, _execute_job
from shipyard.core.evidence import EvidenceRecord, EvidenceStore
from shipyard.core.job import (
    Job,
    TargetResult,
    TargetStatus,
    ValidationMode,
)
from shipyard.core.queue import Queue

if TYPE_CHECKING:
    from pathlib import Path


def _git(cmd: list[str], *, cwd: Path) -> str:
    result = subprocess.run(
        ["git", *cmd], cwd=cwd, capture_output=True, text=True, check=True
    )
    return result.stdout.strip()


@pytest.fixture
def linear_repo(tmp_path: Path, monkeypatch: pytest.MonkeyPatch) -> Path:
    """Two-commit repo; initial touches src/, second touches docs/."""
    repo = tmp_path / "repo"
    repo.mkdir()
    _git(["init", "--initial-branch=main"], cwd=repo)
    _git(["config", "user.email", "t@e.com"], cwd=repo)
    _git(["config", "user.name", "t"], cwd=repo)
    _git(["config", "commit.gpgsign", "false"], cwd=repo)
    (repo / "src").mkdir()
    (repo / "src" / "a.py").write_text("x\n")
    _git(["add", "."], cwd=repo)
    _git(["commit", "-m", "initial"], cwd=repo)
    (repo / "docs").mkdir()
    (repo / "docs" / "README.md").write_text("docs\n")
    _git(["add", "."], cwd=repo)
    _git(["commit", "-m", "docs"], cwd=repo)
    # The reuse module uses ``cwd`` via its ``repo_dir`` argument; our
    # cli wiring omits it so fall back to process cwd.
    monkeypatch.chdir(repo)
    return repo


@dataclass
class _DispatchCount:
    n: int = 0


class _FakeDispatcher:
    """Minimal stand-in for :class:`ExecutorDispatcher`.

    Records each ``validate_target`` call so tests can assert dispatch
    was / was not invoked.
    """

    def __init__(self) -> None:
        self.dispatched: list[str] = []

    def backend_name(self, target_config: dict) -> str:  # noqa: ANN001
        return "local"

    def validate_target(self, **kwargs) -> TargetResult:  # noqa: ANN003
        name = kwargs["target_config"]["name"]
        self.dispatched.append(name)
        now = datetime.now(timezone.utc)
        return TargetResult(
            target_name=name,
            platform=kwargs["target_config"].get("platform", "unknown"),
            status=TargetStatus.PASS,
            backend="local",
            started_at=now,
            completed_at=now,
            duration_secs=1.0,
        )


def _build_ctx(tmp_path: Path) -> Context:
    ctx = Context(json_mode=True)
    ctx._evidence = EvidenceStore(path=tmp_path / "evidence")
    ctx._queue = Queue(state_dir=tmp_path / "queue")
    return ctx


def _config_with_targets(
    tmp_path: Path, targets: dict[str, dict]
) -> object:
    """Build a minimal Config-ish object with ``targets``, ``validation``, and
    ``state_dir`` — the three bits ``_execute_job`` touches.
    """

    class _Cfg:
        validation = {"default": {"build": "echo build"}}

        def __init__(self) -> None:
            self.state_dir = tmp_path / "state"
            self.state_dir.mkdir(exist_ok=True)

        @property
        def targets(self) -> dict[str, dict]:
            return targets

        def get(self, key: str, default=None):  # noqa: ANN001
            return default

    return _Cfg()


def test_reuses_when_diff_misses_globs(
    linear_repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # head = docs commit, ancestor = initial commit. Target cares
    # about src/** but the diff only touches docs/ → reuse.
    head_sha = _git(["rev-parse", "HEAD"], cwd=linear_repo)
    ancestor_sha = _git(["rev-parse", "HEAD~1"], cwd=linear_repo)

    state_tmp = tmp_path / "state"
    ctx = _build_ctx(state_tmp)
    # Seed a passing ancestor evidence record.
    ctx.evidence.record(EvidenceRecord(
        sha=ancestor_sha, branch="main", target_name="mac",
        platform="macos-arm64", status="pass", backend="local",
        completed_at=datetime.now(timezone.utc),
    ))

    targets = {
        "mac": {
            "backend": "local",
            "platform": "macos-arm64",
            "reuse_if_paths_unchanged": ["src/**"],
        }
    }
    config = _config_with_targets(state_tmp, targets)
    # Make the Context return our hand-rolled config.
    monkeypatch.setattr(
        type(ctx), "config", property(lambda self, c=config: c)
    )

    dispatcher = _FakeDispatcher()
    job = Job.create(
        sha=head_sha, branch="feature/x",
        target_names=["mac"], mode=ValidationMode.FULL,
    )

    completed = _execute_job(
        ctx=ctx, job=job, config=config, dispatcher=dispatcher,  # type: ignore[arg-type]
        mode=ValidationMode.FULL, fail_fast=True, resume_from=None,
    )

    assert dispatcher.dispatched == [], "reuse must skip dispatch"
    result = completed.results["mac"]
    assert result.status == TargetStatus.PASS
    assert result.reused_from == ancestor_sha
    assert result.backend == "reused"
    # Evidence record carries reused_from.
    rec = ctx.evidence.get_target("feature/x", "mac")
    assert rec is not None
    assert rec.reused_from == ancestor_sha


def test_dispatches_when_diff_touches_globs(
    linear_repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    # Add a third commit that touches src/** so reuse must refuse.
    (linear_repo / "src" / "a.py").write_text("y\n")
    _git(["add", "."], cwd=linear_repo)
    _git(["commit", "-m", "touch src"], cwd=linear_repo)
    head_sha = _git(["rev-parse", "HEAD"], cwd=linear_repo)
    ancestor_sha = _git(["rev-parse", "HEAD~2"], cwd=linear_repo)

    state_tmp = tmp_path / "state"
    ctx = _build_ctx(state_tmp)
    ctx.evidence.record(EvidenceRecord(
        sha=ancestor_sha, branch="main", target_name="mac",
        platform="macos-arm64", status="pass", backend="local",
        completed_at=datetime.now(timezone.utc),
    ))
    targets = {
        "mac": {
            "backend": "local",
            "platform": "macos-arm64",
            "reuse_if_paths_unchanged": ["src/**"],
        }
    }
    config = _config_with_targets(state_tmp, targets)
    monkeypatch.setattr(
        type(ctx), "config", property(lambda self, c=config: c)
    )

    dispatcher = _FakeDispatcher()
    job = Job.create(
        sha=head_sha, branch="feature/x",
        target_names=["mac"], mode=ValidationMode.FULL,
    )
    completed = _execute_job(
        ctx=ctx, job=job, config=config, dispatcher=dispatcher,  # type: ignore[arg-type]
        mode=ValidationMode.FULL, fail_fast=True, resume_from=None,
    )

    assert dispatcher.dispatched == ["mac"]
    result = completed.results["mac"]
    assert result.reused_from is None


def test_target_without_reuse_config_dispatches_normally(
    linear_repo: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    head_sha = _git(["rev-parse", "HEAD"], cwd=linear_repo)
    ancestor_sha = _git(["rev-parse", "HEAD~1"], cwd=linear_repo)

    state_tmp = tmp_path / "state"
    ctx = _build_ctx(state_tmp)
    ctx.evidence.record(EvidenceRecord(
        sha=ancestor_sha, branch="main", target_name="mac",
        platform="macos-arm64", status="pass", backend="local",
        completed_at=datetime.now(timezone.utc),
    ))
    # No reuse_if_paths_unchanged on the target.
    targets = {"mac": {"backend": "local", "platform": "macos-arm64"}}
    config = _config_with_targets(state_tmp, targets)
    monkeypatch.setattr(
        type(ctx), "config", property(lambda self, c=config: c)
    )

    dispatcher = _FakeDispatcher()
    job = Job.create(
        sha=head_sha, branch="feature/x",
        target_names=["mac"], mode=ValidationMode.FULL,
    )
    completed = _execute_job(
        ctx=ctx, job=job, config=config, dispatcher=dispatcher,  # type: ignore[arg-type]
        mode=ValidationMode.FULL, fail_fast=True, resume_from=None,
    )
    assert dispatcher.dispatched == ["mac"]
    assert completed.results["mac"].reused_from is None
