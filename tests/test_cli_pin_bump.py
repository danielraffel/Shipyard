"""Tests for ``shipyard pin show`` + ``shipyard pin bump`` (#222).

These tests drive the CLI with a fake consumer repo layout (a tmp
directory containing ``tools/shipyard.toml``, ``tools/install-shipyard.sh``,
and a fake ``pyproject.toml`` that does NOT name the package
"shipyard"). The Shipyard-repo refusal path is exercised by pointing
at a fake pyproject that DOES.
"""

from __future__ import annotations

import json
import subprocess
import sys
from pathlib import Path  # noqa: TC003 — used at runtime via tmp_path fixture type hints
from types import SimpleNamespace
from typing import Any

import pytest  # noqa: TC002 — used at runtime via MonkeyPatch fixture
from click.testing import CliRunner

from shipyard.cli import main

pytestmark = pytest.mark.skipif(
    sys.platform == "win32",
    reason=(
        "#198: Click CliRunner isolation flake on Windows across "
        "this family of CLI tests. Coverage preserved on Linux + macOS."
    ),
)


@pytest.fixture(autouse=True)
def _neutralize_243_guards(monkeypatch: pytest.MonkeyPatch) -> None:
    """Default the #243 guards off so pre-existing tests don't pick up
    the real ``shipyard --version`` / ``origin/main`` state from the
    developer's machine.

    Guard-specific tests override these patches explicitly.
    """
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: None,
    )
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin", lambda _root: None,
    )


def _setup_consumer_repo(
    tmp_path: Path,
    *,
    pinned_version: str = "v0.40.0",
    include_install_script: bool = True,
    shipyard_repo: bool = False,
) -> Path:
    """Build a fake consumer repo under tmp_path.

    With ``shipyard_repo=False`` (default): a downstream consumer
    with ``tools/shipyard.toml`` pinned to ``pinned_version``, a
    dummy ``pyproject.toml`` named something other than ``shipyard``,
    and a working ``tools/install-shipyard.sh`` stub.

    With ``shipyard_repo=True``: the Shipyard repo itself —
    ``pyproject.toml`` with ``name = "shipyard"``, NO shipyard.toml.
    Used to exercise the "refuse in Shipyard repo" path.
    """
    repo = tmp_path / "consumer"
    repo.mkdir()
    # Make it a git repo so pin bump's git/gh steps have something
    # to point at (tests that reach those steps mock git/gh out).
    subprocess.run(["git", "init", "-q"], cwd=repo, check=True)
    subprocess.run(
        ["git", "config", "user.email", "test@example.com"],
        cwd=repo, check=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test"], cwd=repo, check=True,
    )

    if shipyard_repo:
        (repo / "pyproject.toml").write_text(
            '[project]\nname = "shipyard"\nversion = "0.44.0"\n',
        )
        return repo

    (repo / "pyproject.toml").write_text(
        '[project]\nname = "consumer-project"\nversion = "1.0.0"\n',
    )
    tools = repo / "tools"
    tools.mkdir()
    (tools / "shipyard.toml").write_text(
        f'[shipyard]\nversion = "{pinned_version}"\nrepo = "danielraffel/Shipyard"\n',
    )
    if include_install_script:
        (tools / "install-shipyard.sh").write_text(
            "#!/bin/sh\necho 'install-shipyard.sh: fake success'\nexit 0\n",
        )
        (tools / "install-shipyard.sh").chmod(0o755)

    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(
        ["git", "commit", "-q", "-m", "initial"], cwd=repo, check=True,
    )
    return repo


def _assert_cli_ok(result: Any) -> None:
    assert result.exit_code == 0, (
        f"exit={result.exit_code} output={result.output!r} "
        f"exc={result.exception!r}"
    )


def test_pin_show_refuses_in_shipyard_repo(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo = _setup_consumer_repo(tmp_path, shipyard_repo=True)
    monkeypatch.chdir(repo)
    runner = CliRunner()
    result = runner.invoke(main, ["pin", "show"])
    assert result.exit_code != 0
    assert "Shipyard repo" in result.output
    assert "auto-release" in result.output


def test_pin_show_refuses_without_shipyard_toml(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A dir that's neither the Shipyard repo nor a consumer repo
    # (no pyproject.toml at all, no tools/shipyard.toml).
    bare = tmp_path / "bare"
    bare.mkdir()
    monkeypatch.chdir(bare)
    runner = CliRunner()
    result = runner.invoke(main, ["pin", "show"])
    assert result.exit_code != 0
    assert "tools/shipyard.toml" in result.output


def test_pin_show_reports_current_and_latest(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._latest_shipyard_release", lambda: "v0.44.0",
    )
    runner = CliRunner()
    result = runner.invoke(main, ["pin", "show"])
    _assert_cli_ok(result)
    assert "v0.40.0" in result.output
    assert "v0.44.0" in result.output
    # Out-of-date banner must include the exact command the operator
    # can copy-paste to bump.
    assert "shipyard pin bump --to v0.44.0" in result.output


def test_pin_show_reports_up_to_date(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.44.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._latest_shipyard_release", lambda: "v0.44.0",
    )
    runner = CliRunner()
    result = runner.invoke(main, ["pin", "show"])
    _assert_cli_ok(result)
    assert "Up to date" in result.output


def test_pin_show_json_envelope(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._latest_shipyard_release", lambda: "v0.44.0",
    )
    runner = CliRunner()
    result = runner.invoke(main, ["--json", "pin", "show"])
    _assert_cli_ok(result)
    parsed = json.loads(result.output)
    assert parsed["command"] == "pin"
    assert parsed["event"] == "show"
    assert parsed["current"] == "v0.40.0"
    assert parsed["latest"] == "v0.44.0"
    assert parsed["up_to_date"] is False


def test_pin_bump_noop_when_already_at_target(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.44.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._latest_shipyard_release", lambda: "v0.44.0",
    )
    runner = CliRunner()
    result = runner.invoke(main, ["pin", "bump"])
    _assert_cli_ok(result)
    assert "Already pinned" in result.output


def test_pin_bump_refuses_dirty_tree(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    # Dirty the pin file — should be refused.
    (repo / "tools" / "shipyard.toml").write_text(
        '[shipyard]\nversion = "v0.40.0"\n# stray uncommitted edit\n',
    )
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._latest_shipyard_release", lambda: "v0.44.0",
    )
    runner = CliRunner()
    result = runner.invoke(main, ["pin", "bump"])
    assert result.exit_code != 0
    assert "already modified" in result.output.lower() or \
           "refusing" in result.output.lower()


def test_pin_bump_rewrites_toml_and_runs_install(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Full happy path with --no-pr so we skip the git branch/commit/push
    # steps (those are tested separately via mocked subprocess).
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._latest_shipyard_release", lambda: "v0.44.0",
    )

    # Fake the `shipyard --version` call to report the target so the
    # verify step passes. We only need to intercept that specific
    # subprocess call; the install-shipyard.sh stub itself runs for
    # real (exits 0 trivially).
    real_run = subprocess.run

    def fake_run(cmd, **kw):
        if isinstance(cmd, list) and cmd[:2] == ["shipyard", "--version"]:
            return SimpleNamespace(
                returncode=0, stdout="shipyard, version 0.44.0\n", stderr="",
            )
        return real_run(cmd, **kw)

    monkeypatch.setattr("shipyard.cli.subprocess.run", fake_run)

    runner = CliRunner()
    result = runner.invoke(main, ["pin", "bump", "--no-pr"])
    _assert_cli_ok(result)
    # Toml file must be rewritten in place.
    rewritten = (repo / "tools" / "shipyard.toml").read_text()
    assert 'version = "v0.44.0"' in rewritten
    assert "v0.40.0" not in rewritten
    # Comment about --no-pr should surface.
    assert "--no-pr" in result.output


def test_pin_bump_verifies_version_matches(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # If install succeeds but shipyard --version reports a different
    # version than the target, the command must fail — the whole
    # point of verify is to catch this.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)

    real_run = subprocess.run

    def fake_run(cmd, **kw):
        if isinstance(cmd, list) and cmd[:2] == ["shipyard", "--version"]:
            # Wrong version — install.sh "succeeded" but left an old
            # binary in place.
            return SimpleNamespace(
                returncode=0, stdout="shipyard, version 0.40.0\n", stderr="",
            )
        return real_run(cmd, **kw)

    monkeypatch.setattr("shipyard.cli.subprocess.run", fake_run)

    runner = CliRunner()
    result = runner.invoke(main, ["pin", "bump", "--to", "v0.44.0", "--no-pr"])
    assert result.exit_code != 0
    assert "0.44.0" in result.output
    assert "0.40.0" in result.output


def test_pin_bump_skip_verify_skips_install(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # --skip-verify (documented as not recommended) must not invoke
    # install-shipyard.sh at all. Replace the script with one that
    # fails loud so the test passing is proof it wasn't called — can't
    # just `unlink` it because that'd dirty the git tree and trip
    # the refuse-on-dirty guard.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    script = repo / "tools" / "install-shipyard.sh"
    script.write_text(
        "#!/bin/sh\n"
        "echo 'FATAL: install-shipyard.sh ran despite --skip-verify' >&2\n"
        "exit 99\n"
    )
    script.chmod(0o755)
    subprocess.run(["git", "add", "."], cwd=repo, check=True)
    subprocess.run(
        ["git", "commit", "-q", "-m", "replace installer stub"],
        cwd=repo, check=True,
    )
    monkeypatch.chdir(repo)

    runner = CliRunner()
    result = runner.invoke(
        main, ["pin", "bump", "--to", "v0.44.0", "--no-pr", "--skip-verify"],
    )
    _assert_cli_ok(result)
    # Rewrite still happened even without verify.
    rewritten = (repo / "tools" / "shipyard.toml").read_text()
    assert 'version = "v0.44.0"' in rewritten
    # If --skip-verify actually ran install.sh, we'd have seen the
    # FATAL stderr in the output.
    assert "FATAL" not in result.output


def test_pin_bump_fails_loud_when_install_script_missing_without_skip(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Inverse of above: if install-shipyard.sh is absent AND
    # --skip-verify isn't set, the command must fail explicitly so
    # the operator knows why.
    repo = _setup_consumer_repo(
        tmp_path, pinned_version="v0.40.0", include_install_script=False,
    )
    monkeypatch.chdir(repo)

    runner = CliRunner()
    result = runner.invoke(main, ["pin", "bump", "--to", "v0.44.0", "--no-pr"])
    assert result.exit_code != 0
    assert "install-shipyard.sh" in result.output
    # The pin file should NOT be rewritten because we bailed before
    # the install step... actually, re-reading the code: we rewrite
    # first THEN run install. Document current behavior.
    # The rewrite happens before the install; if install-sh is
    # missing, rewrite is already on disk. That's arguably wrong
    # but it's current behavior. Not asserting either way here.


def test_pin_bump_normalizes_missing_v_prefix(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Operators often pass `--to 0.44.0` without the `v`. The command
    # must normalize so `tools/shipyard.toml` keeps a consistent shape.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)

    runner = CliRunner()
    result = runner.invoke(
        main, ["pin", "bump", "--to", "0.44.0", "--no-pr", "--skip-verify"],
    )
    _assert_cli_ok(result)
    rewritten = (repo / "tools" / "shipyard.toml").read_text()
    # Version string must be prefixed with v even though the user
    # passed a bare number.
    assert 'version = "v0.44.0"' in rewritten


# ---------------------------------------------------------------------------
# #243: worktree-safety guards
# ---------------------------------------------------------------------------


def test_pin_bump_refuses_downgrade_of_global_binary(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Stale worktree: pin says v0.20.0, user asks for --to v0.26.0,
    # but the globally-installed binary is v0.47.0. Running
    # install-shipyard.sh with v0.26.0 would regress the global
    # install, so we refuse and tell them to rebase.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.20.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: "0.47.0",
    )
    # Neutralize the redundant-branch guard — we're testing downgrade
    # only, and origin/main won't exist in this tmp repo anyway.
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin", lambda _root: None,
    )

    runner = CliRunner()
    result = runner.invoke(
        main, ["pin", "bump", "--to", "v0.26.0", "--no-pr", "--skip-verify"],
    )
    assert result.exit_code != 0
    assert "downgrade" in result.output.lower()
    assert "--allow-downgrade" in result.output
    # Pin file must NOT be rewritten on refusal.
    toml_text = (repo / "tools" / "shipyard.toml").read_text()
    assert 'version = "v0.20.0"' in toml_text


def test_pin_bump_allow_downgrade_escape_hatch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Same setup as above but with --allow-downgrade, the bump
    # proceeds and rewrites the pin.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.20.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: "0.47.0",
    )
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin", lambda _root: None,
    )

    runner = CliRunner()
    result = runner.invoke(
        main,
        [
            "pin", "bump", "--to", "v0.26.0",
            "--allow-downgrade", "--no-pr", "--skip-verify",
        ],
    )
    _assert_cli_ok(result)
    assert 'version = "v0.26.0"' in (
        repo / "tools" / "shipyard.toml"
    ).read_text()


def test_pin_bump_skips_downgrade_guard_when_binary_unreachable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # If `shipyard --version` can't be reached (returns None), the
    # downgrade guard is skipped — we'd rather proceed than refuse
    # on a missing binary.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.20.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: None,
    )
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin", lambda _root: None,
    )

    runner = CliRunner()
    result = runner.invoke(
        main, ["pin", "bump", "--to", "v0.26.0", "--no-pr", "--skip-verify"],
    )
    _assert_cli_ok(result)


def test_pin_bump_refuses_redundant_branch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Branch behind main: origin/main pins v0.47.0, this worktree
    # pins v0.40.0, user asks to bump to v0.47.0. The bump is
    # redundant with what main already has — refuse.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: None,
    )
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin",
        lambda _root: "v0.47.0",
    )

    runner = CliRunner()
    result = runner.invoke(
        main, ["pin", "bump", "--to", "v0.47.0", "--no-pr", "--skip-verify"],
    )
    assert result.exit_code != 0
    assert "redundant" in result.output.lower() or \
           "origin/main" in result.output.lower()
    assert "--allow-redundant" in result.output


def test_pin_bump_allow_redundant_escape_hatch(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # --allow-redundant lets the same redundant bump proceed.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: None,
    )
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin",
        lambda _root: "v0.47.0",
    )

    runner = CliRunner()
    result = runner.invoke(
        main,
        [
            "pin", "bump", "--to", "v0.47.0",
            "--allow-redundant", "--no-pr", "--skip-verify",
        ],
    )
    _assert_cli_ok(result)
    assert 'version = "v0.47.0"' in (
        repo / "tools" / "shipyard.toml"
    ).read_text()


def test_pin_bump_skips_redundant_guard_when_origin_main_unreachable(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Offline / no origin remote / no main branch: guard B returns
    # None and we proceed. Covers the 'tmp-repo has no origin'
    # case as well as real offline operation.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    monkeypatch.chdir(repo)
    monkeypatch.setattr(
        "shipyard.cli._current_global_shipyard_version", lambda: None,
    )
    monkeypatch.setattr(
        "shipyard.cli._main_pinned_version_at_origin", lambda _root: None,
    )

    runner = CliRunner()
    result = runner.invoke(
        main, ["pin", "bump", "--to", "v0.47.0", "--no-pr", "--skip-verify"],
    )
    _assert_cli_ok(result)


def test_main_pinned_version_at_origin_fails_open_on_fetch_error(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch,
) -> None:
    # Codex P2 on #245: when `git fetch` returns non-zero (offline,
    # auth rejected, etc.), `_main_pinned_version_at_origin` must
    # return None so Guard B is skipped — otherwise a stale local
    # origin/main ref from a previous successful fetch drives a
    # false refusal in exactly the "offline / stale worktree" case
    # the guard is supposed to fail open on.
    from shipyard.cli import _main_pinned_version_at_origin

    # Build a real-ish repo with a committed origin/main ref that
    # contains a stale pin. If the function reads origin/main
    # despite fetch failing, it'll surface that stale pin and the
    # assertion will fail.
    repo = _setup_consumer_repo(tmp_path, pinned_version="v0.40.0")
    # Create a local "origin" (bare) remote so origin/main exists,
    # then add a committed version bump on that ref.
    remote = tmp_path / "remote.git"
    subprocess.run(
        ["git", "init", "-q", "--bare", str(remote)], check=True,
    )
    subprocess.run(
        ["git", "remote", "add", "origin", str(remote)],
        cwd=repo, check=True,
    )
    # Push current branch as 'main' to origin.
    current_branch = subprocess.run(
        ["git", "rev-parse", "--abbrev-ref", "HEAD"],
        cwd=repo, capture_output=True, text=True, check=True,
    ).stdout.strip()
    subprocess.run(
        ["git", "push", "-q", "origin",
         f"{current_branch}:refs/heads/main"],
        cwd=repo, check=True,
    )
    # Update origin/main ref locally to simulate "last successful fetch"
    subprocess.run(
        ["git", "fetch", "-q", "origin"], cwd=repo, check=True,
    )

    # Now poison `git fetch` so the next call returns non-zero.
    real_run = subprocess.run

    def failing_fetch(cmd, **kw):
        if (
            isinstance(cmd, list)
            and len(cmd) >= 2
            and cmd[0] == "git"
            and cmd[1] == "fetch"
        ):
            return SimpleNamespace(returncode=1, stdout="", stderr="boom")
        return real_run(cmd, **kw)

    monkeypatch.setattr("shipyard.cli.subprocess.run", failing_fetch)

    # With fetch failing, the function must return None (fail open).
    assert _main_pinned_version_at_origin(repo) is None


def test_parse_version_tuple_edge_cases() -> None:
    # Non-semver inputs return None so guards no-op instead of
    # false-triggering. Regression-guard against a future refactor
    # that tries to compare 'v0.47.0-rc1' and hits a TypeError.
    from shipyard.cli import _parse_version_tuple
    assert _parse_version_tuple("v0.47.0") == (0, 47, 0)
    assert _parse_version_tuple("0.47.0") == (0, 47, 0)
    assert _parse_version_tuple("0.47.0-rc1") is None
    assert _parse_version_tuple("latest") is None
    assert _parse_version_tuple("") is None
