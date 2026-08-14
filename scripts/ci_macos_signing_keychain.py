#!/usr/bin/env python3
"""Create and remove a CI signing keychain without changing user state permanently."""

from __future__ import annotations

import argparse
import json
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Any


def _security(*args: str, home: Path | None = None) -> subprocess.CompletedProcess[str]:
    env = None
    if home is not None:
        env = os.environ.copy()
        env["HOME"] = str(home)
    return subprocess.run(
        ["security", *args],
        check=False,
        capture_output=True,
        text=True,
        env=env,
    )


def _require_security(*args: str, home: Path | None = None) -> subprocess.CompletedProcess[str]:
    result = _security(*args, home=home)
    if result.returncode != 0:
        raise RuntimeError(f"security {args[0]} failed with status {result.returncode}")
    return result


def _parse_keychain_output(output: str) -> list[str]:
    try:
        return shlex.split(output, posix=True)
    except ValueError as error:
        raise RuntimeError("security returned malformed keychain output") from error


def _write_state(path: Path, state: dict[str, Any]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(f".{path.name}.tmp")
    temporary.write_text(json.dumps(state), encoding="utf-8")
    os.chmod(temporary, 0o600)
    temporary.replace(path)


def prepare(keychain: Path, state_path: Path, signing_home: Path) -> None:
    """Snapshot user keychain state before installing the ephemeral keychain."""
    state: dict[str, Any] = {
        "snapshot_complete": False,
        "keychain": str(keychain),
    }
    _write_state(state_path, state)

    default_result = _security("default-keychain", "-d", "user")
    list_result = _security("list-keychains", "-d", "user")
    if default_result.returncode != 0 or list_result.returncode != 0:
        raise RuntimeError("could not snapshot the user keychain configuration")

    defaults = _parse_keychain_output(default_result.stdout)
    keychains = _parse_keychain_output(list_result.stdout)
    if len(defaults) != 1:
        raise RuntimeError("security returned an unusable default keychain snapshot")
    if not keychains:
        raise RuntimeError("security returned an unusable keychain search list snapshot")

    state.update(
        snapshot_complete=True,
        default_keychain=defaults[0],
        search_list=keychains,
    )
    _write_state(state_path, state)

    _require_security("create-keychain", "-p", "ci", str(keychain))
    (signing_home / "Library" / "Preferences").mkdir(parents=True, exist_ok=True)
    _require_security(
        "list-keychains", "-d", "user", "-s", str(keychain), home=signing_home
    )
    isolated_list = _require_security(
        "list-keychains", "-d", "user", home=signing_home
    )
    if _parse_keychain_output(isolated_list.stdout) != [str(keychain)]:
        raise RuntimeError("isolated signing keychain search list did not persist")
    _require_security("unlock-keychain", "-p", "ci", str(keychain))
    _require_security("set-keychain-settings", "-t", "21600", "-u", str(keychain))


def restore(keychain: Path, state_path: Path, signing_home: Path) -> None:
    """Verify global state stayed unchanged and delete the ephemeral keychain."""
    failures: list[str] = []
    state: dict[str, Any] | None = None
    try:
        state = json.loads(state_path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        failures.append("saved keychain state is unavailable")

    if state and state.get("snapshot_complete") is True:
        search_list = state.get("search_list")
        default_keychain = state.get("default_keychain")
        if not isinstance(search_list, list) or not all(
            isinstance(item, str) for item in search_list
        ):
            failures.append("saved keychain search list is invalid")

        if not isinstance(default_keychain, str) or not default_keychain:
            failures.append("saved default keychain is invalid")

        verified_list = _security("list-keychains", "-d", "user")
        verified_default = _security("default-keychain", "-d", "user")
        if verified_list.returncode != 0 or verified_default.returncode != 0:
            failures.append("could not verify the user keychain configuration")
        else:
            try:
                actual_list = _parse_keychain_output(verified_list.stdout)
                actual_defaults = _parse_keychain_output(verified_default.stdout)
            except RuntimeError:
                failures.append("keychain verification output was malformed")
            else:
                if actual_list != search_list:
                    failures.append("user keychain search list changed during signing")
                if actual_defaults != [default_keychain]:
                    failures.append("user default keychain changed during signing")

    # The path is generated from RUNNER_TEMP and GITHUB_RUN_ID. Deletion is
    # safe to retry and must not be skipped when setup failed partway through.
    deletion = _security("delete-keychain", str(keychain))
    if deletion.returncode != 0:
        failures.append("could not delete the ephemeral signing keychain")
    shutil.rmtree(signing_home, ignore_errors=True)

    if failures:
        raise RuntimeError("; ".join(failures))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("prepare", "restore"))
    parser.add_argument("--keychain", type=Path, required=True)
    parser.add_argument("--state", type=Path, required=True)
    parser.add_argument("--signing-home", type=Path, required=True)
    args = parser.parse_args(argv)

    try:
        if args.action == "prepare":
            prepare(args.keychain, args.state, args.signing_home)
        else:
            restore(args.keychain, args.state, args.signing_home)
    except RuntimeError as error:
        print(f"macOS signing keychain {args.action} failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
