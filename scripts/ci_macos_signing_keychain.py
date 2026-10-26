#!/usr/bin/env python3
"""Create and remove a CI signing keychain without changing user state permanently."""

from __future__ import annotations

import argparse
import json
import os
import pwd
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


def _user_home() -> Path:
    """Resolve the service user's home independently of inherited ``HOME``.

    launchd services can inherit an empty or stale HOME even though the
    process has the correct user identity. The `security -d user` commands
    use HOME to locate that user's keychain preferences, so make the identity
    explicit while leaving the keychain contents untouched.
    """
    return Path(pwd.getpwuid(os.getuid()).pw_dir)


def prepare(keychain: Path, state_path: Path, signing_home: Path) -> None:
    """Snapshot user keychain state before installing the ephemeral keychain.

    A launchd service account may have no user-domain keychain at all. That is
    a valid empty baseline because signing runs entirely under ``signing_home``
    with an explicit ephemeral keychain.
    """
    state: dict[str, Any] = {
        "snapshot_complete": False,
        "keychain_creation_started": False,
        "keychain_created": False,
        "keychain": str(keychain),
    }
    _write_state(state_path, state)

    user_home = _user_home()
    default_result = _security("default-keychain", "-d", "user", home=user_home)
    list_result = _security("list-keychains", "-d", "user", home=user_home)
    keychains = _parse_keychain_output(list_result.stdout) if list_result.returncode == 0 else []
    no_user_keychain = (
        default_result.returncode != 0
        and list_result.returncode == 0
        and not keychains
        and "default keychain could not be found" in default_result.stderr.lower()
    )
    if list_result.returncode != 0 or (default_result.returncode != 0 and not no_user_keychain):
        failures = []
        for name, result in (("default-keychain", default_result), ("list-keychains", list_result)):
            if result.returncode != 0:
                detail = result.stderr.strip()
                failures.append(f"{name} exited {result.returncode}" + (f": {detail}" if detail else ""))
        raise RuntimeError("could not snapshot the user keychain configuration (" + "; ".join(failures) + ")")

    if no_user_keychain:
        state.update(snapshot_complete=True, snapshot_mode="none", default_keychain=None, search_list=[])
    else:
        defaults = _parse_keychain_output(default_result.stdout)
        if len(defaults) != 1:
            raise RuntimeError("security returned an unusable default keychain snapshot")
        if not keychains:
            raise RuntimeError("security returned an unusable keychain search list snapshot")
        state.update(snapshot_complete=True, snapshot_mode="user", default_keychain=defaults[0], search_list=keychains)
    _write_state(state_path, state)

    state["keychain_creation_started"] = True
    _write_state(state_path, state)
    _require_security("create-keychain", "-p", "ci", str(keychain))
    state["keychain_created"] = True
    _write_state(state_path, state)
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
        parsed_state = json.loads(state_path.read_text(encoding="utf-8"))
        if isinstance(parsed_state, dict):
            state = parsed_state
        else:
            failures.append("saved keychain state is invalid")
    except (OSError, json.JSONDecodeError):
        failures.append("saved keychain state is unavailable")

    if state and state.get("snapshot_complete") is True and state.get("snapshot_mode", "user") != "none":
        search_list = state.get("search_list")
        default_keychain = state.get("default_keychain")
        if not isinstance(search_list, list) or not all(
            isinstance(item, str) for item in search_list
        ):
            failures.append("saved keychain search list is invalid")

        if not isinstance(default_keychain, str) or not default_keychain:
            failures.append("saved default keychain is invalid")

        user_home = _user_home()
        verified_list = _security("list-keychains", "-d", "user", home=user_home)
        verified_default = _security("default-keychain", "-d", "user", home=user_home)
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
    # The state records whether creation started and completed. This avoids
    # deleting a stale path when snapshot failed before creation, while still
    # retrying cleanup after a crash during or after `create-keychain`.
    creation_started = state is None or state.get("keychain_creation_started") is True
    keychain_created = state is None or state.get("keychain_created") is True
    # State from older runs predates the explicit creation markers; a complete
    # snapshot was always followed by creation in that version.
    if state and "keychain_created" not in state and state.get("snapshot_complete") is True:
        keychain_created = True
    if (creation_started or keychain_created) and keychain.exists():
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
