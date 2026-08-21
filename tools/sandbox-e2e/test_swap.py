from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import pytest

from shipyard_sandbox import (
    BINARY_NAME,
    PYTHON_BINARY_NAME,
    PythonShipyardSource,
    Sandbox,
    parse_advertised_commands,
    parse_top_level_commands,
)

REPO_ROOT = Path(__file__).resolve().parents[2]


@pytest.mark.smoke
def test_help_banner_exits_zero_with_usage_output(sandbox: Sandbox) -> None:
    result = sandbox.run(["--help"]).expect_success()

    assert "Usage:" in result.stdout
    assert "Commands:" in result.stdout
    assert "shipyard" in result.stdout


@pytest.mark.smoke
def test_version_exits_zero_with_version_string(sandbox: Sandbox) -> None:
    result = sandbox.run(["--version"]).expect_success()

    assert "shipyard" in result.combined_output


@pytest.mark.smoke
def test_unknown_subcommand_exits_nonzero_with_output(sandbox: Sandbox) -> None:
    result = sandbox.run(["frobnicate-totally-fake"])

    assert result.returncode != 0
    assert result.combined_output.strip()
    assert "frobnicate-totally-fake" in result.combined_output


@pytest.mark.smoke
def test_unknown_nested_subcommand_exits_nonzero_with_output(sandbox: Sandbox) -> None:
    result = sandbox.run(["cloud", "bogus-subcmd-totally-fake"])

    assert result.returncode != 0
    assert result.combined_output.strip()
    assert "bogus-subcmd-totally-fake" in result.combined_output


@pytest.mark.surface
def test_every_advertised_top_level_command_help_is_nonsilent(sandbox: Sandbox) -> None:
    root = sandbox.run(["--help"]).expect_success()
    commands = parse_top_level_commands(root.stdout)

    assert {"paths", "pin", "doctor", "cloud", "daemon", "ship-state"}.issubset(commands)
    failures: list[str] = []
    for command in sorted(commands):
        result = sandbox.run([command, "--help"])
        if result.returncode == 0 and result.combined_output.strip():
            continue
        if result.returncode != 0 and result.combined_output.strip():
            continue
        failures.append(
            f"{command} --help returned {result.returncode} with no output"
        )
    assert not failures, "\n".join(failures)


@pytest.mark.surface
def test_every_advertised_command_path_help_exits_zero_in_sandbox(
    sandbox: Sandbox,
) -> None:
    visited: set[tuple[str, ...]] = set()
    failures: list[str] = []

    def walk(path: tuple[str, ...]) -> None:
        if path in visited:
            return
        visited.add(path)
        args = [*path] if path[-1:] == ("help",) else [*path, "--help"]
        result = sandbox.run(args, timeout=30)
        if result.returncode != 0 or not result.combined_output.strip():
            failures.append(
                f"{' '.join(args) or '--help'} returned "
                f"{result.returncode} with output={result.combined_output!r}"
            )
            return
        if path[-1:] == ("help",):
            return
        if len(path) >= 5:
            failures.append(f"command tree exceeded expected depth: {' '.join(path)}")
            return
        for command in sorted(parse_advertised_commands(result.combined_output)):
            walk((*path, command))

    walk(())

    expected_paths = {
        ("cloud", "handoff", "list-stuck"),
        ("cloud", "handoff", "run"),
        ("release-bot", "hook", "run"),
        ("ship-state", "reconcile"),
        ("targets", "warm", "drain"),
        ("governance", "status"),
    }
    assert expected_paths.issubset(visited)
    assert not failures, "\n".join(failures)


@pytest.mark.state
def test_json_paths_resolve_inside_sandbox_home(sandbox: Sandbox) -> None:
    result = sandbox.run(["--json", "paths"]).expect_success()
    payload = result.json_stdout()

    assert isinstance(payload, dict)
    assert payload["mode"] == "isolated"
    assert payload["binary_name"] == "shipyard"
    for key in ["global_dir", "state_dir", "daemon_dir", "daemon_socket", "daemon_pid_file"]:
        value = Path(payload[key])
        assert value.is_relative_to(sandbox.home_dir), f"{key} escaped sandbox: {value}"
    assert "shipyard-dev" in payload["state_dir"]


@pytest.mark.state
def test_ship_state_list_empty_sandbox_is_nonsilent(sandbox: Sandbox) -> None:
    result = sandbox.run(["ship-state", "list"]).expect_success()

    assert result.combined_output.strip()
    assert "No active ship state" in result.combined_output


@pytest.mark.daemon
def test_daemon_status_does_not_start_daemon(sandbox: Sandbox) -> None:
    result = sandbox.run(["--json", "daemon", "status"]).expect_success()
    payload = result.json_stdout()

    assert isinstance(payload, dict)
    assert payload["running"] is False
    assert not (sandbox.home_dir / "Library" / "Application Support" / "shipyard-dev" / "daemon" / "daemon.sock").exists()


@pytest.mark.daemon
@pytest.mark.skipif(sys.platform == "win32", reason="daemon IPC runtime is Unix-only")
def test_daemon_refresh_spawns_detached_child_and_reuses_repos(sandbox: Sandbox) -> None:
    env = {
        "HOME": str(sandbox.home_dir),
        "USERPROFILE": str(sandbox.home_dir),
        "PATH": f"{sandbox.bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
        "XDG_CONFIG_HOME": str(sandbox.home_dir / ".config"),
        "XDG_STATE_HOME": str(sandbox.home_dir / ".local" / "state"),
        "XDG_CACHE_HOME": str(sandbox.home_dir / ".cache"),
        "RUST_BACKTRACE": "1",
    }

    with tempfile.TemporaryDirectory(prefix="syrd-") as short_state:
        state_dir = Path(short_state)

        def run_daemon(args: list[str]) -> dict[str, object]:
            result = subprocess.run(
                [
                    str(sandbox.binary_path),
                    "--mode",
                    "isolated",
                    "--state-dir",
                    str(state_dir),
                    "--json",
                    "daemon",
                    *args,
                ],
                cwd=sandbox.work_dir,
                env=env,
                check=True,
                capture_output=True,
                text=True,
                timeout=10,
            )
            return json.loads(result.stdout)

        def wait_for_running() -> dict[str, object]:
            deadline = time.monotonic() + 3
            last: dict[str, object] | None = None
            while time.monotonic() < deadline:
                last = run_daemon(["status"])
                if last.get("running") is True:
                    return last
                time.sleep(0.1)
            raise AssertionError(f"daemon did not become reachable: {last}")

        def seed_registrations(repos: list[str]) -> None:
            daemon_dir = state_dir / "daemon"
            daemon_dir.mkdir(parents=True, exist_ok=True)
            payload = [
                {"repo": repo, "hook_id": index + 1}
                for index, repo in enumerate(repos)
            ]
            (daemon_dir / "registrations.json").write_text(
                json.dumps(payload, indent=2),
                encoding="utf-8",
            )

        try:
            seed_registrations(["owner/daemon-refresh"])
            first = run_daemon(["refresh", "--repo", "owner/daemon-refresh"])
            assert first["command"] == "daemon:refresh"
            assert first["stopped_prior"] is False
            assert first["repos"] == ["owner/daemon-refresh"]
            assert isinstance(first["new_pid"], int)

            status = wait_for_running()
            assert status["registered_repos"] == ["owner/daemon-refresh"]

            second = run_daemon(["refresh"])
            assert second["command"] == "daemon:refresh"
            assert second["stopped_prior"] is True
            assert second["repos"] == ["owner/daemon-refresh"]
            assert isinstance(second["new_pid"], int)

            status = wait_for_running()
            assert isinstance(status["registered_repos"], list)
        finally:
            subprocess.run(
                [
                    str(sandbox.binary_path),
                    "--mode",
                    "isolated",
                    "--state-dir",
                    str(state_dir),
                    "--json",
                    "daemon",
                    "stop",
                ],
                cwd=sandbox.work_dir,
                env=env,
                check=False,
                capture_output=True,
                text=True,
                timeout=10,
            )


@pytest.mark.smoke
def test_pin_show_outside_consumer_repo_fails_loudly(sandbox: Sandbox) -> None:
    result = sandbox.run(["pin", "show"])

    assert result.returncode != 0
    assert result.combined_output.strip()
    assert "tools/shipyard.toml" in result.combined_output


@pytest.mark.installer
def test_install_script_dry_run_defaults_to_production_names(sandbox: Sandbox) -> None:
    result = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_DRY_RUN": "1",
        },
        check=True,
        capture_output=True,
        text=True,
    )

    assert "ARTIFACT_PREFIX=shipyard" in result.stdout
    assert "BINARY_NAME=shipyard" in result.stdout
    assert "ALIAS_NAME=sy" in result.stdout
    assert "COMPAT_NAME=" not in result.stdout


@pytest.mark.installer
def test_install_script_supports_private_release_repo_token() -> None:
    content = (REPO_ROOT / "install.sh").read_text(encoding="utf-8")

    assert "SHIPYARD_GITHUB_TOKEN" in content
    assert "printf 'Authorization: Bearer %s\\n'" in content
    assert '"${GITHUB_TOKEN_VALUE}"' in content
    assert "Accept: application/octet-stream" in content
    assert "select_asset_url" in content
    assert "curl_shipyard -sL" in content


@pytest.mark.installer
def test_install_script_intel_mac_guard_is_version_aware(sandbox: Sandbox) -> None:
    latest = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_DRY_RUN": "1",
            "SHIPYARD_INSTALL_TEST_UNAME_S": "Darwin",
            "SHIPYARD_INSTALL_TEST_UNAME_M": "x86_64",
        },
        check=False,
        capture_output=True,
        text=True,
    )
    assert latest.returncode == 2
    assert "Intel Macs (x86_64) are not supported" in latest.stderr
    assert "SHIPYARD_VERSION=v0.49.0" in latest.stderr

    old_pin = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_DRY_RUN": "1",
            "SHIPYARD_VERSION": "v0.49.0",
            "SHIPYARD_INSTALL_TEST_UNAME_S": "Darwin",
            "SHIPYARD_INSTALL_TEST_UNAME_M": "x86_64",
        },
        check=True,
        capture_output=True,
        text=True,
    )
    assert "OS=macos" in old_pin.stdout
    assert "ARCH=x64" in old_pin.stdout
    assert "ARTIFACT=shipyard-macos-x64" in old_pin.stdout
    assert "VERSION_LABEL=v0.49.0" in old_pin.stdout


@pytest.mark.installer
def test_install_script_skip_download_preserves_production_binary_names(
    sandbox: Sandbox,
) -> None:
    install_dir = sandbox.home_dir / "install-bin"
    install_dir.mkdir()
    shutil.copy2(sandbox.binary_path, install_dir / BINARY_NAME)

    result = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_INSTALL_DIR": str(install_dir),
            "SHIPYARD_SKIP_DOWNLOAD": "1",
        },
        check=True,
        capture_output=True,
        text=True,
    )

    assert f"Installed {BINARY_NAME}" in result.stdout
    assert (install_dir / BINARY_NAME).exists()
    assert (install_dir / "sy").is_symlink()


@pytest.mark.installer
@pytest.mark.skipif(sys.platform == "win32", reason="uses POSIX shell fakes")
def test_install_script_replaces_existing_binary_atomically(sandbox: Sandbox) -> None:
    install_dir = sandbox.home_dir / "install-bin-atomic"
    install_dir.mkdir()
    binary = install_dir / BINARY_NAME
    binary.write_text(
        "#!/bin/sh\n"
        "echo 'shipyard 0.0.1-old'\n",
        encoding="utf-8",
    )
    binary.chmod(0o755)
    old_inode = binary.stat().st_ino

    fake_curl = sandbox.bin_dir / "curl"
    fake_curl.write_text(
        "#!/bin/sh\n"
        "out=''\n"
        "write_out=0\n"
        "while [ \"$#\" -gt 0 ]; do\n"
        "  case \"$1\" in\n"
        "    -o) out=\"$2\"; shift 2 ;;\n"
        "    -w) write_out=1; shift 2 ;;\n"
        "    -*) shift ;;\n"
        "    *) shift ;;\n"
        "  esac\n"
        "done\n"
        "if [ -n \"$out\" ]; then\n"
        "  cat > \"$out\" <<'SCRIPT'\n"
        "#!/bin/sh\n"
        "echo 'shipyard 9.9.9'\n"
        "SCRIPT\n"
        "  exit 0\n"
        "fi\n"
        "cat <<'JSON'\n"
        "{\"assets\":[{\"name\":\"shipyard-linux-arm64\",\"browser_download_url\":\"https://example.invalid/shipyard-linux-arm64\"}]}\n"
        "JSON\n"
        "if [ \"$write_out\" -eq 1 ]; then printf '200\\n'; fi\n",
        encoding="utf-8",
    )
    fake_curl.chmod(0o755)

    result = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": f"{sandbox.bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_INSTALL_DIR": str(install_dir),
            "SHIPYARD_CURL_BIN": str(fake_curl),
            "SHIPYARD_VERSION": "v9.9.9",
            "SHIPYARD_INSTALL_TEST_UNAME_S": "Linux",
            "SHIPYARD_INSTALL_TEST_UNAME_M": "aarch64",
        },
        check=True,
        capture_output=True,
        text=True,
    )

    assert "Installed shipyard" in result.stdout
    assert binary.stat().st_ino != old_inode
    assert (
        subprocess.check_output([str(binary), "--version"], text=True).strip()
        == "shipyard 9.9.9"
    )
    assert not list(install_dir.glob(f".{BINARY_NAME}.install.*"))


@pytest.mark.installer
@pytest.mark.skipif(sys.platform == "win32", reason="uses POSIX shell fakes")
def test_install_script_adhoc_fallback_recovers_notarized_launch_failure(
    sandbox: Sandbox,
) -> None:
    install_dir = sandbox.home_dir / "install-bin-adhoc"
    install_dir.mkdir()
    marker = sandbox.work_dir / "adhoc-marker"
    log = sandbox.work_dir / "codesign.log"
    binary = install_dir / BINARY_NAME
    binary.write_text(
        "#!/bin/sh\n"
        "if [ -f \"$SHIPYARD_FAKE_ADHOC_MARKER\" ]; then\n"
        "  echo 'shipyard 0.1.0'\n"
        "  exit 0\n"
        "fi\n"
        "echo 'simulated taskgated rejection' >&2\n"
        "exit 137\n",
        encoding="utf-8",
    )
    binary.chmod(0o755)
    fake_codesign = sandbox.bin_dir / "codesign"
    fake_codesign.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$*\" >> \"$SHIPYARD_FAKE_CODESIGN_LOG\"\n"
        "case \"$*\" in\n"
        "  *'-dv '*) echo 'TeamIdentifier=TEAMID' >&2; exit 0 ;;\n"
        "  *'--force --sign - '*) touch \"$SHIPYARD_FAKE_ADHOC_MARKER\"; exit 0 ;;\n"
        "  *'--remove-signature '*) exit 0 ;;\n"
        "  *) exit 0 ;;\n"
        "esac\n",
        encoding="utf-8",
    )
    fake_codesign.chmod(0o755)

    result = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": f"{sandbox.bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_INSTALL_DIR": str(install_dir),
            "SHIPYARD_SKIP_DOWNLOAD": "1",
            "SHIPYARD_INSTALL_TEST_UNAME_S": "Darwin",
            "SHIPYARD_INSTALL_TEST_UNAME_M": "arm64",
            "SHIPYARD_FAKE_ADHOC_MARKER": str(marker),
            "SHIPYARD_FAKE_CODESIGN_LOG": str(log),
        },
        check=True,
        capture_output=True,
        text=True,
    )

    assert "WARN: notarized binary would not launch" in result.stderr
    assert marker.exists()
    assert "Installed shipyard" in result.stdout
    codesign_log = log.read_text(encoding="utf-8")
    assert "--remove-signature" in codesign_log
    assert "--force --sign -" in codesign_log


@pytest.mark.installer
@pytest.mark.skipif(sys.platform == "win32", reason="uses POSIX shell fakes")
def test_install_script_can_disable_adhoc_fallback(sandbox: Sandbox) -> None:
    install_dir = sandbox.home_dir / "install-bin-no-adhoc"
    install_dir.mkdir()
    marker = sandbox.work_dir / "adhoc-marker-disabled"
    binary = install_dir / BINARY_NAME
    binary.write_text(
        "#!/bin/sh\n"
        "echo 'simulated taskgated rejection' >&2\n"
        "exit 137\n",
        encoding="utf-8",
    )
    binary.chmod(0o755)
    fake_codesign = sandbox.bin_dir / "codesign"
    fake_codesign.write_text(
        "#!/bin/sh\n"
        "case \"$*\" in\n"
        "  *'-dv '*) echo 'TeamIdentifier=TEAMID' >&2; exit 0 ;;\n"
        "  *'--force --sign - '*) touch \"$SHIPYARD_FAKE_ADHOC_MARKER\"; exit 0 ;;\n"
        "  *) exit 0 ;;\n"
        "esac\n",
        encoding="utf-8",
    )
    fake_codesign.chmod(0o755)

    result = subprocess.run(
        ["bash", str(REPO_ROOT / "install.sh")],
        cwd=sandbox.work_dir,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": f"{sandbox.bin_dir}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_INSTALL_DIR": str(install_dir),
            "SHIPYARD_SKIP_DOWNLOAD": "1",
            "SHIPYARD_INSTALL_TEST_UNAME_S": "Darwin",
            "SHIPYARD_INSTALL_TEST_UNAME_M": "arm64",
            "SHIPYARD_NO_ADHOC_FALLBACK": "1",
            "SHIPYARD_FAKE_ADHOC_MARKER": str(marker),
        },
        check=False,
        capture_output=True,
        text=True,
    )

    assert result.returncode == 1
    assert "failed post-install smoke" in result.stderr
    assert not marker.exists()


@pytest.mark.installer
def test_pin_bump_runs_consumer_install_wrapper_and_verifies_version(
    sandbox: Sandbox,
) -> None:
    consumer = sandbox.work_dir / "consumer"
    tools = consumer / "tools"
    install_bin = consumer / ".tmp-shipyard-bin"
    tools.mkdir(parents=True)
    install_bin.mkdir()
    (tools / "shipyard.toml").write_text(
        '[shipyard]\nversion = "v0.0.9"\nrepo = "danielraffel/Shipyard"\n',
        encoding="utf-8",
    )
    installer = tools / "install-shipyard.sh"
    installer.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        'cp "$SHIPYARD_BINARY_FOR_TEST" "$SHIPYARD_INSTALL_DIR/shipyard"\n'
        'chmod +x "$SHIPYARD_INSTALL_DIR/shipyard"\n',
        encoding="utf-8",
    )
    installer.chmod(0o755)
    subprocess.run(["git", "init", "--quiet", "--initial-branch=main"], cwd=consumer, check=True)
    subprocess.run(["git", "config", "user.email", "sandbox@example.test"], cwd=consumer, check=True)
    subprocess.run(["git", "config", "user.name", "Sandbox"], cwd=consumer, check=True)
    subprocess.run(["git", "add", "."], cwd=consumer, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "seed"], cwd=consumer, check=True)
    version_output = sandbox.run(["--version"]).expect_success().stdout.strip()
    target_version = f"v{version_output.rsplit(maxsplit=1)[-1]}"

    result = subprocess.run(
        [
            str(sandbox.binary_path),
            "pin",
            "bump",
            "--to",
            target_version,
            "--no-pr",
            "--allow-downgrade",
            "--allow-redundant",
        ],
        cwd=consumer,
        env={
            "HOME": str(sandbox.home_dir),
            "PATH": f"{install_bin}:/usr/bin:/bin:/usr/sbin:/sbin",
            "SHIPYARD_BINARY_FOR_TEST": str(sandbox.binary_path),
            "SHIPYARD_INSTALL_DIR": str(install_bin),
        },
        check=True,
        capture_output=True,
        text=True,
    )

    assert "--no-pr: edit left in the working tree" in result.stdout
    assert (
        f'version = "{target_version}"'
        in (tools / "shipyard.toml").read_text(encoding="utf-8")
    )
    assert (install_bin / "shipyard").exists()


@pytest.mark.pin
@pytest.mark.skipif(sys.platform == "win32", reason="uses a POSIX fake gh wrapper")
def test_pin_bump_opens_pr_through_isolated_git_and_fake_gh(sandbox: Sandbox) -> None:
    remote = sandbox.work_dir / "consumer-origin.git"
    consumer = sandbox.work_dir / "consumer-pr"
    tools = consumer / "tools"
    gh_args = sandbox.work_dir / "fake-gh-args.txt"

    subprocess.run(
        ["git", "init", "--bare", "--quiet", "--initial-branch=main", str(remote)],
        check=True,
    )
    consumer.mkdir()
    subprocess.run(["git", "init", "--quiet", "--initial-branch=main"], cwd=consumer, check=True)
    subprocess.run(["git", "config", "user.email", "sandbox@example.test"], cwd=consumer, check=True)
    subprocess.run(["git", "config", "user.name", "Sandbox"], cwd=consumer, check=True)
    tools.mkdir(parents=True)
    (tools / "shipyard.toml").write_text(
        '[shipyard]\nversion = "v0.0.9"\nrepo = "danielraffel/Shipyard"\n',
        encoding="utf-8",
    )
    (tools / "install-shipyard.sh").write_text("#!/bin/sh\nexit 0\n", encoding="utf-8")
    subprocess.run(["git", "add", "."], cwd=consumer, check=True)
    subprocess.run(["git", "commit", "-q", "-m", "seed"], cwd=consumer, check=True)
    subprocess.run(["git", "remote", "add", "origin", str(remote)], cwd=consumer, check=True)
    subprocess.run(["git", "push", "-q", "-u", "origin", "main"], cwd=consumer, check=True)

    fake_gh = sandbox.bin_dir / "gh"
    fake_gh.write_text(
        "#!/bin/sh\n"
        "printf '%s\\n' \"$@\" > \"$SHIPYARD_FAKE_GH_ARGS\"\n"
        "echo 'https://github.com/example/consumer/pull/123'\n",
        encoding="utf-8",
    )
    fake_gh.chmod(0o755)

    result = sandbox.run(
        [
            "--json",
            "--cwd",
            str(consumer),
            "pin",
            "bump",
            "--to",
            "v0.1.0",
            "--skip-verify",
            "--allow-downgrade",
            "--allow-redundant",
        ],
        extra_env={"SHIPYARD_FAKE_GH_ARGS": str(gh_args)},
    ).expect_success()
    payload = result.json_stdout()

    assert payload["command"] == "pin"
    assert payload["result"] == "pr-opened"
    assert payload["from"] == "v0.0.9"
    assert payload["to"] == "v0.1.0"
    assert payload["pr_url"] == "https://github.com/example/consumer/pull/123"
    assert 'version = "v0.1.0"' in (tools / "shipyard.toml").read_text(encoding="utf-8")
    assert (
        subprocess.run(
            ["git", "branch", "--show-current"],
            cwd=consumer,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()
        == "chore/bump-shipyard-pin-to-v0.1.0"
    )
    assert subprocess.run(
        ["git", "show-ref", "--verify", "refs/heads/chore/bump-shipyard-pin-to-v0.1.0"],
        cwd=remote,
        check=False,
        capture_output=True,
        text=True,
    ).returncode == 0
    gh_text = gh_args.read_text(encoding="utf-8")
    assert "pr\ncreate\n" in gh_text
    assert "chore: bump Shipyard pin v0.0.9 -> v0.1.0" in gh_text


@pytest.mark.parity
def test_json_ship_state_list_has_contract_keys(sandbox: Sandbox) -> None:
    result = sandbox.run(["--json", "ship-state", "list"]).expect_success()
    payload = json.loads(result.stdout)

    assert payload["schema_version"] == 1
    assert payload["command"] == "ship-state:list"
    assert payload["states"] == []


@pytest.mark.cross_binary
def test_python_and_rust_empty_ship_state_list_contract_match(
    sandbox: Sandbox,
    python_shipyard_source: PythonShipyardSource,
) -> None:
    sandbox.stage_python_shipyard(python_shipyard_source)

    rust = sandbox.run(["--json", "ship-state", "list"]).expect_success().json_stdout()
    python = (
        sandbox.run(["--json", "ship-state", "list"], binary=PYTHON_BINARY_NAME)
        .expect_success()
        .json_stdout()
    )

    assert python == rust == {
        "schema_version": 1,
        "command": "ship-state:list",
        "states": [],
    }


@pytest.mark.cross_binary
def test_python_and_rust_daemon_status_contract_match(
    sandbox: Sandbox,
    python_shipyard_source: PythonShipyardSource,
) -> None:
    sandbox.stage_python_shipyard(python_shipyard_source)

    rust = sandbox.run(["--json", "daemon", "status"]).expect_success().json_stdout()
    python = (
        sandbox.run(["--json", "daemon", "status"], binary=PYTHON_BINARY_NAME)
        .expect_success()
        .json_stdout()
    )

    assert python == rust == {
        "schema_version": 1,
        "command": "daemon:status",
        "running": False,
    }


@pytest.mark.cross_binary
def test_python_and_rust_doctor_safe_shape_match(
    sandbox: Sandbox,
    python_shipyard_source: PythonShipyardSource,
) -> None:
    sandbox.stage_python_shipyard(python_shipyard_source)

    rust = sandbox.run(["--json", "doctor"], timeout=30).expect_success().json_stdout()
    python = (
        sandbox.run(["--json", "doctor"], binary=PYTHON_BINARY_NAME, timeout=30)
        .expect_success()
        .json_stdout()
    )

    for payload in [python, rust]:
        assert payload["schema_version"] == 1
        assert payload["command"] == "doctor"
        assert isinstance(payload["ready"], bool)
        checks = payload["checks"]
        assert {"Core", "Cloud providers"}.issubset(checks)
        assert {"git", "ssh", "shipyard-on-path", "rich-bundle"}.issubset(
            checks["Core"]
        )
        assert {"gh", "nsc"}.issubset(checks["Cloud providers"])


@pytest.mark.cross_binary
def test_python_and_rust_pin_show_failure_mentions_consumer_pin(
    sandbox: Sandbox,
    python_shipyard_source: PythonShipyardSource,
) -> None:
    sandbox.stage_python_shipyard(python_shipyard_source)

    for binary in [PYTHON_BINARY_NAME, BINARY_NAME]:
        result = sandbox.run(["pin", "show"], binary=binary)
        assert result.returncode != 0
        assert "tools/shipyard.toml" in result.combined_output


@pytest.mark.bulkhead
@pytest.mark.parametrize(
    "args",
    [
        ["ship"],
        ["run"],
        ["auto-merge", "1"],
        ["cloud", "add-lane", "--pr", "1", "--target", "linux"],
        ["cloud", "retarget", "--pr", "1", "--target", "linux", "--provider", "namespace"],
        ["cloud", "handoff", "run", "123", "--to", "namespace"],
        ["daemon", "start"],
        ["daemon", "run"],
        ["daemon", "refresh"],
        ["daemon", "stop"],
        ["wait", "run", "123"],
        ["watch"],
        ["--json", "wait", "run", "123"],
        ["--state-dir", "/tmp/shipyard-sandbox", "daemon", "start"],
    ],
)
def test_destructive_commands_are_blocked_by_harness(
    sandbox: Sandbox,
    args: list[str],
) -> None:
    with pytest.raises(AssertionError):
        sandbox.run(args)
