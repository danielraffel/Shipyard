"""Regression: every executor must accept `resume_from` and `mode` kwargs.

Stage 1 dogfooding of `shipyard run` against Pulp's real SSH
targets surfaced a bug: the CLI dispatch path forwards
`resume_from` (and now `mode`, for prepared-state) to every
backend, but only LocalExecutor's signature accepted those
kwargs. An SSH run crashed with
`TypeError: SSHExecutor.validate() got an unexpected keyword
argument 'resume_from'` after the mac local target had already
passed, because the CLI ran local first and then tried to
dispatch the same kwargs to SSH.

This file pins the contract: every executor in
`shipyard.executor.dispatch._resolve_executor` must accept
`resume_from` and `mode` without raising, even if it ignores
them internally.
"""

from __future__ import annotations

import inspect

from shipyard.executor.cloud import CloudExecutor
from shipyard.executor.local import LocalExecutor
from shipyard.executor.ssh import SSHExecutor
from shipyard.executor.ssh_windows import SSHWindowsExecutor


def _validate_params(cls) -> set[str]:
    return set(inspect.signature(cls.validate).parameters.keys())


def test_local_accepts_resume_from_and_mode() -> None:
    params = _validate_params(LocalExecutor)
    assert "resume_from" in params
    assert "mode" in params


def test_ssh_accepts_resume_from_and_mode() -> None:
    params = _validate_params(SSHExecutor)
    assert "resume_from" in params
    assert "mode" in params


def test_ssh_windows_accepts_resume_from_and_mode() -> None:
    params = _validate_params(SSHWindowsExecutor)
    assert "resume_from" in params
    assert "mode" in params


def test_cloud_accepts_resume_from_and_mode() -> None:
    params = _validate_params(CloudExecutor)
    assert "resume_from" in params
    assert "mode" in params
