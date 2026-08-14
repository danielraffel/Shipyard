#!/usr/bin/env python3
from __future__ import annotations

import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import ci_macos_signing_keychain as subject


def completed(stdout: str = "", returncode: int = 0) -> subprocess.CompletedProcess[str]:
    return subprocess.CompletedProcess(["security"], returncode, stdout, "")


class SigningKeychainTests(unittest.TestCase):
    def test_security_isolates_preferences_with_explicit_home(self) -> None:
        with mock.patch.object(
            subject.subprocess, "run", return_value=completed()
        ) as run:
            subject._security("list-keychains", "-d", "user", home=Path("/tmp/isolated home"))
        self.assertEqual(run.call_args.kwargs["env"]["HOME"], "/tmp/isolated home")

    def test_prepare_never_mutates_default_or_search_list_and_handles_spaces(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            keychain = root / "ephemeral signing.keychain-db"
            state = root / "state.json"
            default = "/Users/ci/Library/Keychains/login keychain.keychain-db"
            search = [default, "/Users/ci/Library/Keychains/pulp-signing.keychain-db"]
            responses = [
                completed(f'"{default}"'),
                completed("\n".join(f'"{item}"' for item in search)),
                completed(),
                completed(),
                completed(f'"{keychain}"'),
                completed(),
                completed(),
            ]
            with mock.patch.object(subject, "_security", side_effect=responses) as security:
                subject.prepare(keychain, state, root / "signing home")
            calls = security.call_args_list
            self.assertTrue((root / "signing home/Library/Preferences").is_dir())
            self.assertFalse(
                any("-s" in call.args and call.kwargs.get("home") is None for call in calls)
            )
            self.assertIn(
                mock.call(
                    "list-keychains",
                    "-d",
                    "user",
                    "-s",
                    str(keychain),
                    home=root / "signing home",
                ),
                calls,
            )
            self.assertIn(
                mock.call(
                    "list-keychains",
                    "-d",
                    "user",
                    home=root / "signing home",
                ),
                calls,
            )
            self.assertEqual(json.loads(state.read_text())["search_list"], search)

            verify = [
                completed("\n".join(f'"{item}"' for item in search)),
                completed(f'"{default}"'),
                completed(),
            ]
            with mock.patch.object(subject, "_security", side_effect=verify) as security:
                subject.restore(keychain, state, root / "signing home")
            self.assertEqual(
                security.call_args_list,
                [
                    mock.call("list-keychains", "-d", "user"),
                    mock.call("default-keychain", "-d", "user"),
                    mock.call("delete-keychain", str(keychain)),
                ],
            )

    def test_unavailable_snapshot_fails_before_keychain_creation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            with mock.patch.object(
                subject, "_security", side_effect=[completed(returncode=1), completed()]
            ) as security:
                with self.assertRaisesRegex(RuntimeError, "could not snapshot"):
                    subject.prepare(root / "keychain", root / "state", root / "home")
            self.assertEqual(security.call_count, 2)

    def test_missing_state_cleanup_only_deletes_and_reports_state_loss(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            keychain = root / "keychain"
            with mock.patch.object(subject, "_security", return_value=completed()) as security:
                with self.assertRaisesRegex(RuntimeError, "state is unavailable"):
                    subject.restore(keychain, root / "missing", root / "home")
            security.assert_called_once_with("delete-keychain", str(keychain))

    def test_changed_global_state_fails_without_overwriting_it(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / "state"
            state.write_text(json.dumps({
                "snapshot_complete": True,
                "default_keychain": "/original",
                "search_list": ["/original", "/signing"],
            }))
            responses = [completed('"/changed"'), completed('"/changed"'), completed()]
            with mock.patch.object(subject, "_security", side_effect=responses) as security:
                with self.assertRaisesRegex(RuntimeError, "changed during signing"):
                    subject.restore(root / "ephemeral", state, root / "home")
            self.assertFalse(any("-s" in call.args for call in security.call_args_list))

    def test_failed_ephemeral_deletion_is_a_cleanup_failure(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = root / "state"
            state.write_text(json.dumps({
                "snapshot_complete": True,
                "default_keychain": "/original",
                "search_list": ["/original"],
            }))
            responses = [completed('"/original"'), completed('"/original"'), completed(returncode=1)]
            with mock.patch.object(subject, "_security", side_effect=responses):
                with self.assertRaisesRegex(RuntimeError, "could not delete"):
                    subject.restore(root / "ephemeral", state, root / "home")


if __name__ == "__main__":
    unittest.main()
