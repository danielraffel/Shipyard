#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path


WRAPPER = Path(__file__).with_name("ghapp")


class GhappWrapperTests(unittest.TestCase):
    def test_privileged_grammar_matches_verified_fleet_native_help(self) -> None:
        wrapper = WRAPPER.read_text(encoding="utf-8")
        match = re.search(r"GHAPP_COMMAND_GRAMMAR='(.*?)'\n\n", wrapper, re.S)
        self.assertIsNotNone(match)
        normalized: list[str] = []
        for line in match.group(1).splitlines():
            fields = line.split("|", 3)
            if fields[0] not in {"boolean", "value"} or fields[1] == "*":
                continue
            normalized.extend(
                f"{fields[1]}|{fields[2]}|{option}|{fields[0]}"
                for option in fields[3].split()
            )
        digest = hashlib.sha256(
            ("\n".join(sorted(normalized)) + "\n").encode()
        ).hexdigest()
        fixture = WRAPPER.with_name("fixtures") / "ghapp-native-help-digests.tsv"
        verified = {
            version: expected
            for version, expected in (
                line.split("\t", 1)
                for line in fixture.read_text(encoding="utf-8").splitlines()
                if line and not line.startswith("#")
            )
        }

        self.assertEqual(set(verified), {"2.93.0", "2.96.0"})
        self.assertTrue(all(expected == digest for expected in verified.values()))

    def test_every_supported_long_boolean_preserves_following_repo(self) -> None:
        wrapper = WRAPPER.read_text(encoding="utf-8")
        grammar = re.search(
            r"GHAPP_COMMAND_GRAMMAR='(.*?)'\n\n", wrapper, re.S
        ).group(1)
        cases = []
        for line in grammar.splitlines():
            fields = line.split("|", 3)
            if fields[0] != "boolean" or fields[1] == "*":
                continue
            cases.extend(
                (fields[1], fields[2], option)
                for option in fields[3].split()
                if option.startswith("--")
            )
        for command, subcommand, option in cases:
            with self.subTest(command=command, subcommand=subcommand, option=option):
                arguments = [command]
                if subcommand != "*":
                    arguments.append(subcommand)
                arguments.extend((option, "--repo", "owner/repo"))
                result = self.run_wrapper(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_every_supported_long_value_flag_cannot_swallow_repo_selector(self) -> None:
        wrapper = WRAPPER.read_text(encoding="utf-8")
        grammar = re.search(
            r"GHAPP_COMMAND_GRAMMAR='(.*?)'\n\n", wrapper, re.S
        ).group(1)
        cases = []
        for line in grammar.splitlines():
            fields = line.split("|", 3)
            if fields[0] != "value":
                continue
            cases.extend(
                (fields[1], fields[2], option)
                for option in fields[3].split()
                if option.startswith("--")
            )
        for command, subcommand, option in cases:
            with self.subTest(command=command, subcommand=subcommand, option=option):
                self.helper_log.unlink(missing_ok=True)
                self.gh_log.unlink(missing_ok=True)
                arguments = [command]
                if subcommand != "*":
                    arguments.append(subcommand)
                arguments.extend((option, "--repo", "owner/repo"))
                result = self.run_wrapper(*arguments)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"{option} requires a value before --repo", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.wrapper = self.bin / "ghapp"
        self.wrapper.write_bytes(WRAPPER.read_bytes())
        self.wrapper.chmod(0o755)
        self.helper = self.root / "helper"
        self.gh = self.root / "gh"
        self.shipyard = self.bin / "shipyard"
        self.helper_log = self.root / "helper-args"
        self.gh_log = self.root / "gh-args"
        self.gh_env_log = self.root / "gh-env"
        self.resolver_log = self.root / "resolver-args"
        self.private_key = self.root / "github-app.pem"
        self.resolver_context = self.wrapper.with_name(
            f"{self.wrapper.name}.shipyard-context.json"
        )
        self.guard_log = self.root / "guard-args"
        self.helper.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "with open(os.environ['HELPER_LOG'], 'a', encoding='utf-8') as log:\n"
            "    log.write(' '.join(sys.argv[1:]) + '\\n')\n"
            "print(json.dumps({'token': 'ghs_private_fixture', "
            "'kind': 'github-app-installation', "
            "'expires_at': '2099-01-01T00:00:00Z'}, separators=(',', ':')))\n",
            encoding="utf-8",
        )
        self.gh.write_text(
            "#!/bin/sh\n"
            "[ \"${GH_TOKEN:-${GH_ENTERPRISE_TOKEN:-}}\" = ghs_private_fixture ] || exit 92\n"
            "printf '%s\\n' \"$*\" > \"$GH_LOG\"\n"
            "printf '%s|%s|%s|%s\\n' \"${GH_HOST:-}\" \"${GH_TOKEN:-}\" \"${GH_ENTERPRISE_TOKEN:-}\" \"${GH_REPO:-}\" > \"$GH_ENV_LOG\"\n"
            "printf 'ok\\n'\n",
            encoding="utf-8",
        )
        self.shipyard.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, sys\n"
            "with open(os.environ['RESOLVER_LOG'], 'a', encoding='utf-8') as log:\n"
            "    log.write(' '.join(sys.argv[1:]) + '\\n')\n"
            "wrapper = sys.argv[sys.argv.index('--wrapper') + 1]\n"
            "repo = sys.argv[sys.argv.index('--repo') + 1]\n"
            "payload = {\n"
            "    'schema_version': 1,\n"
            "    'command': 'auth.helper-argv',\n"
            "    'wrapper': wrapper,\n"
            "    'repo': repo,\n"
            "    'credential_argv': ['--app-id', '123456', '--private-key', os.environ['PRIVATE_KEY']],\n"
            "}\n"
            "mode = os.environ.get('RESOLVER_MODE')\n"
            "if mode == 'extra-key': payload['unexpected'] = True\n"
            "if mode == 'schema-bool': payload['schema_version'] = True\n"
            "if mode == 'oversize': payload['credential_argv'][1] = '1' * 17000\n"
            "if mode and mode.startswith('app-id:'): payload['credential_argv'][1] = mode.split(':', 1)[1]\n"
            "if mode and mode.startswith('private-key:'): payload['credential_argv'][3] = mode.split(':', 1)[1]\n"
            "print(json.dumps(payload, separators=(',', ':')))\n",
            encoding="utf-8",
        )
        self.helper.chmod(0o755)
        self.gh.chmod(0o755)
        self.shipyard.chmod(0o755)
        guards = self.root / "guards"
        guards.mkdir()
        close_guard = guards / "pr-close-guard"
        close_guard.write_text(
            "#!/bin/sh\n"
            "printf '%s\\n' \"$*\" >> \"$GUARD_LOG\"\n",
            encoding="utf-8",
        )
        close_guard.chmod(0o755)
        self.environment = {
            **os.environ,
            "SHIPYARD_GITHUB_APP_TOKEN_HELPER": str(self.helper),
            "SHIPYARD_GITHUB_APP_CACHE_DIR": str(self.root / "cache"),
            "SHIPYARD_GHAPP_GH_BINARY": str(self.gh),
            "SHIPYARD_GHAPP_PYTHON_BINARY": sys.executable,
            "SHIPYARD_GHAPP_GUARDS_DIR": str(self.root / "guards"),
            "HELPER_LOG": str(self.helper_log),
            "GH_LOG": str(self.gh_log),
            "GH_ENV_LOG": str(self.gh_env_log),
            "GUARD_LOG": str(self.guard_log),
            "GH_EXPECTED": str(self.gh),
            "RESOLVER_LOG": str(self.resolver_log),
            "PRIVATE_KEY": str(self.private_key),
        }
        self.environment.pop("GH_TOKEN", None)
        self.environment.pop("GITHUB_TOKEN", None)
        self.environment.pop("GH_HOST", None)
        self.write_resolver_context()

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def run_wrapper(
        self, *args: str, timeout: float | None = None
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [str(self.wrapper), *args],
            cwd=self.root,
            env=self.environment,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )

    def write_resolver_context(self, **overrides: object) -> None:
        payload: dict[str, object] = {
            "schema_version": 1,
            "mode": "shipyard",
            "global_dir": str(self.root / "governed global"),
        }
        payload.update(overrides)
        self.resolver_context.write_text(
            json.dumps(payload, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        self.resolver_context.chmod(0o600)

    def install_generation_wrapper(self) -> list[Path]:
        home = self.root / "home"
        local = home / ".local"
        share = local / "share"
        shipyard = share / "shipyard"
        generation_store = shipyard / "auth-generations"
        generation = generation_store / ("a" * 64)
        generation.mkdir(parents=True)
        for ancestor in (home, local, share):
            ancestor.chmod(0o755)
        for private_dir in (shipyard, generation_store, generation):
            private_dir.chmod(0o700)

        generation_wrapper = generation / "ghapp"
        generation_wrapper.write_bytes(WRAPPER.read_bytes())
        generation_helper = generation / "shipyard-github-app-token"
        generation_helper.write_bytes(self.helper.read_bytes())
        generation_binary = generation / "shipyard"
        generation_binary.write_bytes(self.shipyard.read_bytes())
        generation_close_guard = generation / "pr-close-guard"
        generation_close_guard.write_bytes((self.root / "guards/pr-close-guard").read_bytes())
        for executable in (
            generation_wrapper,
            generation_helper,
            generation_binary,
            generation_close_guard,
        ):
            executable.chmod(0o700)
        context = generation / "ghapp.shipyard-context.json"
        context.write_text(
            json.dumps(
                {
                    "schema_version": 2,
                    "mode": "shipyard",
                    "global_dir": str(self.root / "governed global"),
                    "authority_identity": "b" * 64,
                    "generation_id": "a" * 64,
                },
                separators=(",", ":"),
            )
            + "\n",
            encoding="utf-8",
        )
        context.chmod(0o600)

        self.wrapper.unlink()
        self.wrapper.symlink_to(generation_wrapper)
        self.environment["HOME"] = str(home)
        return [home, local, share, shipyard, generation_store, generation]

    def install_public_trampoline(self) -> tuple[Path, Path, Path]:
        self.install_generation_wrapper()
        selected_wrapper = Path(os.readlink(self.wrapper))
        home = Path(self.environment["HOME"])
        public_bin = home / ".local/bin"
        public_bin.mkdir()
        public_bin.chmod(0o755)
        public_wrapper = public_bin / "ghapp"
        public_wrapper.write_bytes(WRAPPER.read_bytes())
        public_wrapper.chmod(0o700)
        selector = public_wrapper.with_name("ghapp.shipyard-generation")
        selector.symlink_to(selected_wrapper)
        self.wrapper = public_wrapper
        return public_wrapper, selector, selected_wrapper

    def test_public_trampoline_selects_safe_immutable_generation(self) -> None:
        _, _, selected_wrapper = self.install_public_trampoline()

        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.wrapper.is_symlink())
        self.assertTrue(selected_wrapper.is_file())

    def test_public_trampoline_rejects_missing_selector(self) -> None:
        _, selector, _ = self.install_public_trampoline()
        selector.unlink()

        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("generation selector is missing or unsafe", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_public_trampoline_rejects_non_symlink_selector(self) -> None:
        _, selector, selected_wrapper = self.install_public_trampoline()
        selector.unlink()
        selector.write_text(str(selected_wrapper), encoding="utf-8")

        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("generation selector is missing or unsafe", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_public_trampoline_rejects_selector_outside_private_store(self) -> None:
        _, selector, _ = self.install_public_trampoline()
        outside = self.root / ("f" * 64) / "ghapp"
        outside.parent.mkdir()
        outside.write_bytes(WRAPPER.read_bytes())
        outside.chmod(0o700)
        selector.unlink()
        selector.symlink_to(outside)

        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("selector escapes the private store", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_generation_wrapper_accepts_safe_public_ancestors(self) -> None:
        self.install_generation_wrapper()

        result = self.run_wrapper(
            "token", "--repo", "danielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"kind":"github-app-installation"', result.stdout)

    def test_generation_wrapper_accepts_gnu_stat_fallback(self) -> None:
        self.install_generation_wrapper()
        generation_wrapper = self.wrapper.resolve()
        gnu_stat = shutil.which("gstat")
        native_is_gnu = subprocess.run(
            ["/usr/bin/stat", "--version"],
            text=True,
            capture_output=True,
            check=False,
        ).returncode == 0
        if not native_is_gnu:
            if gnu_stat is None:
                self.skipTest("GNU stat is unavailable on this BSD-stat host")
            generation_wrapper.write_text(
                generation_wrapper.read_text(encoding="utf-8").replace(
                    "/usr/bin/stat", gnu_stat
                ),
                encoding="utf-8",
            )
            generation_wrapper.chmod(0o700)

        result = self.run_wrapper(
            "token", "--repo", "danielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"kind":"github-app-installation"', result.stdout)

    def test_generation_wrapper_binds_the_file_opened_before_selector_swap(self) -> None:
        self.install_generation_wrapper()
        generation_a = self.wrapper.resolve().parent
        generation_b = generation_a.parent / ("b" * 64)
        shutil.copytree(generation_a, generation_b)
        context_b = generation_b / "ghapp.shipyard-context.json"
        payload = json.loads(context_b.read_text(encoding="utf-8"))
        payload["generation_id"] = "b" * 64
        context_b.write_text(
            json.dumps(payload, separators=(",", ":")) + "\n",
            encoding="utf-8",
        )
        context_b.chmod(0o600)
        for generation, marker in ((generation_a, "A"), (generation_b, "B")):
            binary = generation / "shipyard"
            binary.write_text(
                binary.read_text(encoding="utf-8").replace(
                    "log.write(' '.join(sys.argv[1:]) + '\\n')",
                    f"log.write('{marker} ' + ' '.join(sys.argv[1:]) + '\\n')",
                    1,
                ),
                encoding="utf-8",
            )
            binary.chmod(0o700)
        started = self.root / "wrapper-opened"
        release = self.root / "release-wrapper"
        self.environment["SHIPYARD_TEST_RACE_STARTED"] = str(started)
        self.environment["SHIPYARD_TEST_RACE_RELEASE"] = str(release)
        blocker = (
            '/usr/bin/touch "$SHIPYARD_TEST_RACE_STARTED"\n'
            'while [[ ! -e "$SHIPYARD_TEST_RACE_RELEASE" ]]; do '
            "/bin/sleep 0.01; done\n"
        )
        original = generation_a.joinpath("ghapp").read_text(encoding="utf-8")

        def run_race(wrapper_text: str) -> subprocess.CompletedProcess[str]:
            generation_a.joinpath("ghapp").write_text(
                wrapper_text.replace("set -euo pipefail\n", f"set -euo pipefail\n{blocker}", 1),
                encoding="utf-8",
            )
            generation_a.joinpath("ghapp").chmod(0o700)
            self.wrapper.unlink()
            self.wrapper.symlink_to(generation_a / "ghapp")
            started.unlink(missing_ok=True)
            release.unlink(missing_ok=True)
            process = subprocess.Popen(
                [str(self.wrapper), "pr", "view", "--repo", "danielraffel/Shipyard"],
                cwd=self.root,
                env=self.environment,
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
            )
            deadline = time.monotonic() + 5
            while not started.exists() and process.poll() is None:
                if time.monotonic() >= deadline:
                    process.kill()
                    self.fail("wrapper did not reach the selector race boundary")
                time.sleep(0.01)
            next_selector = self.wrapper.with_name("ghapp.next")
            next_selector.symlink_to(generation_b / "ghapp")
            os.replace(next_selector, self.wrapper)
            release.touch()
            stdout, stderr = process.communicate(timeout=5)
            return subprocess.CompletedProcess(
                process.args, process.returncode, stdout, stderr
            )

        result = run_race(original)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(
            self.resolver_log.read_text(encoding="utf-8").startswith("A "),
            "running wrapper did not retain its opened generation",
        )

        self.resolver_log.unlink()
        selector_reread = original.replace(
            'if [[ -z "$opened_wrapper_path" || ! -f "$opened_wrapper_path" || \\\n',
            'opened_wrapper_path="$(/usr/bin/readlink "$public_wrapper_path")"\n'
            'if [[ -z "$opened_wrapper_path" || ! -f "$opened_wrapper_path" || \\\n',
            1,
        )
        self.assertNotEqual(selector_reread, original, "mutation control did not apply")
        mutation = run_race(selector_reread)
        self.assertEqual(mutation.returncode, 0, mutation.stderr)
        self.assertTrue(
            self.resolver_log.read_text(encoding="utf-8").startswith("B "),
            "selector-reread mutation did not reproduce mixed-generation binding",
        )

    def test_generation_wrapper_rejects_writable_ancestor_chain(self) -> None:
        ancestors = self.install_generation_wrapper()
        for ancestor in ancestors:
            with self.subTest(ancestor=ancestor):
                original_mode = ancestor.stat().st_mode & 0o777
                ancestor.chmod(0o733)
                result = self.run_wrapper(
                    "token", "--repo", "danielraffel/Shipyard"
                )
                ancestor.chmod(original_mode)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("malformed or unsafe", result.stderr)
                self.assertFalse(self.helper_log.exists())

    def test_generation_wrapper_rejects_symlinked_ancestor_chain(self) -> None:
        ancestors = self.install_generation_wrapper()
        for index in range(len(ancestors) - 1, -1, -1):
            ancestor = ancestors[index]
            with self.subTest(ancestor=ancestor):
                real_ancestor = ancestor.with_name(f"{ancestor.name}-real")
                ancestor.rename(real_ancestor)
                ancestor.symlink_to(real_ancestor)
                result = self.run_wrapper(
                    "token", "--repo", "danielraffel/Shipyard"
                )
                ancestor.unlink()
                real_ancestor.rename(ancestor)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("malformed or unsafe", result.stderr)
                self.assertFalse(self.helper_log.exists())

    def test_token_mode_preserves_json_helper_contract(self) -> None:
        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('"kind":"github-app-installation"', result.stdout)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())
        self.assertIn("--api-url https://api.github.com", self.helper_log.read_text())
        self.assertFalse(self.resolver_log.exists())

    def test_token_mode_preserves_explicit_no_disk_cache(self) -> None:
        self.environment["SHIPYARD_GITHUB_APP_CACHE_DIR"] = ""

        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertNotIn("--cache-dir", helper_args)
        self.assertIn("--repo danielraffel/Shipyard", helper_args)

    def test_token_mode_rejects_api_host_override_before_helper(self) -> None:
        self.environment["GITHUB_API_URL"] = "https://attacker.example"

        result = self.run_wrapper(
            "token",
            "--api-url",
            "https://attacker.example",
            "--repo",
            "danielraffel/Shipyard",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pinned", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_token_mode_rejects_abbreviated_api_host_override(self) -> None:
        result = self.run_wrapper(
            "token",
            "--api-u",
            "https://attacker.example",
            "--repo",
            "danielraffel/Shipyard",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pinned", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_token_mode_ignores_ambient_api_host(self) -> None:
        self.environment["GITHUB_API_URL"] = "https://attacker.example"

        result = self.run_wrapper("token", "--repo", "danielraffel/Shipyard")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--api-url https://api.github.com", self.helper_log.read_text())

    def test_token_mode_rejects_multiple_repo_selectors(self) -> None:
        result = self.run_wrapper(
            "token",
            "--repo",
            "owner/one",
            "--repo",
            "owner/two",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exactly one", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_cli_mode_routes_api_endpoint_to_matching_repository(self) -> None:
        result = self.run_wrapper(
            "api",
            "repos/Generous-Corp/pulp/hooks",
            "--jq",
            "length",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "ok\n")
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo Generous-Corp/pulp", helper_args)
        self.assertIn("--app-id 123456", helper_args)
        self.assertIn(f"--private-key {self.private_key}", helper_args)
        self.assertEqual(
            self.resolver_log.read_text(),
            f"--mode shipyard --global-dir {self.root / 'governed global'} auth helper-argv --wrapper {self.wrapper} --repo Generous-Corp/pulp\n",
        )
        self.assertNotIn("ghs_private_fixture", helper_args)
        self.assertEqual(
            self.gh_log.read_text(),
            "api repos/Generous-Corp/pulp/hooks --jq length\n",
        )

    def test_cli_mode_requires_release_matched_sibling_resolver(self) -> None:
        self.shipyard.unlink()

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("sibling shipyard resolver is unavailable", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_generation_wrapper_refuses_missing_sibling_close_guard(self) -> None:
        self.install_generation_wrapper()
        self.wrapper.resolve().parent.joinpath("pr-close-guard").unlink()

        result = self.run_wrapper("auth", "status", "--repo", "danielraffel/Shipyard")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("installed generation member is malformed or unsafe", result.stderr)
        self.assertFalse(self.gh_log.exists())

    def test_generation_wrapper_keeps_optional_merge_guard_global(self) -> None:
        self.install_generation_wrapper()
        guard = self.root / "guards/merge-guard"
        guard.write_text("#!/bin/sh\nexit 73\n", encoding="utf-8")
        guard.chmod(0o755)

        result = self.run_wrapper(
            "pr", "merge", "7", "--repo", "danielraffel/Shipyard"
        )

        # Guard refusals retain the wrapper's established fail-closed exit
        # contract rather than leaking an implementation-specific guard code.
        self.assertEqual(result.returncode, 1)
        self.assertFalse(self.gh_log.exists())

    def test_cli_mode_uses_exact_fleet_resolver_context(self) -> None:
        self.write_resolver_context(
            mode="isolated", global_dir=str(self.root / "governed global")
        )

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.resolver_log.read_text(),
            " ".join(
                (
                    "--mode isolated --global-dir",
                    str(self.root / "governed global"),
                    "auth helper-argv --wrapper",
                    str(self.wrapper),
                    "--repo Generous-Corp/pulp\n",
                )
            ),
        )

    def test_cli_mode_rejects_unsafe_context_file_before_resolver(self) -> None:
        cases = {
            "boolean-schema": {"schema_version": True},
            "extra-key": {"unexpected": True},
            "invalid-mode": {"mode": "foreign"},
            "oversized-global-dir": {"global_dir": "/" + "a" * 4096},
            "control-character": {"global_dir": "/tmp/bad\npath"},
            "c1-control-character": {"global_dir": "/tmp/bad\u0085path"},
            "duplicate-separator": {"global_dir": "/tmp//global"},
            "dot-component": {"global_dir": "/tmp/./global"},
        }
        for name, overrides in cases.items():
            with self.subTest(name=name):
                self.write_resolver_context(**overrides)
                result = self.run_wrapper(
                    "api", "repos/Generous-Corp/pulp/hooks"
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("resolver context is malformed or unsafe", result.stderr)
                self.assertFalse(self.resolver_log.exists())
                self.assertFalse(self.helper_log.exists())

    def test_cli_mode_rejects_context_with_open_mode_before_resolver(self) -> None:
        self.write_resolver_context()
        self.resolver_context.chmod(0o644)

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolver context is malformed or unsafe", result.stderr)
        self.assertFalse(self.resolver_log.exists())

    def test_cli_mode_rejects_symlink_context_before_resolver(self) -> None:
        target = self.root / "context-target.json"
        self.resolver_context = target
        self.write_resolver_context()
        self.resolver_context = self.wrapper.with_name(
            f"{self.wrapper.name}.shipyard-context.json"
        )
        self.resolver_context.unlink()
        self.resolver_context.symlink_to(target)

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolver context is malformed or unsafe", result.stderr)
        self.assertFalse(self.resolver_log.exists())

    def test_cli_mode_rejects_fifo_context_without_blocking(self) -> None:
        self.resolver_context.unlink()
        os.mkfifo(self.resolver_context, mode=0o600)

        result = self.run_wrapper(
            "api", "repos/Generous-Corp/pulp/hooks", timeout=2.0
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolver context is malformed or unsafe", result.stderr)
        self.assertFalse(self.resolver_log.exists())

    def test_cli_mode_requires_context_for_manual_or_fleet_install(self) -> None:
        self.resolver_context.unlink()

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolver context is required for direct mode", result.stderr)
        self.assertFalse(self.resolver_log.exists())
        self.assertFalse(self.helper_log.exists())

    def test_cli_mode_refuses_context_disappearance_after_shell_precheck(self) -> None:
        raced_context = self.root / "context-raced-away.json"
        race_hook = self.root / "remove-resolver-context.py"
        race_hook.write_text(
            "import os\n"
            "os.replace(os.environ['RACE_CONTEXT'], os.environ['RACE_DEST'])\n",
            encoding="utf-8",
        )
        wrapper_source = self.wrapper.read_text(encoding="utf-8")
        parser_boundary = "fi\nresolver_context_argv=()\n"
        self.assertEqual(wrapper_source.count(parser_boundary), 1)
        self.wrapper.write_text(
            wrapper_source.replace(
                parser_boundary,
                f'fi\n"$python_binary" -I "{race_hook}"\nresolver_context_argv=()\n',
                1,
            ),
            encoding="utf-8",
        )
        self.environment["RACE_CONTEXT"] = str(self.resolver_context)
        self.environment["RACE_DEST"] = str(raced_context)

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("resolver context is malformed or unsafe", result.stderr)
        self.assertFalse(self.resolver_context.exists())
        self.assertTrue(raced_context.is_file())
        self.assertFalse(self.resolver_log.exists())
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_absolute_shell_env_and_git_ignore_hostile_resolution(self) -> None:
        hostile = self.root / "hostile-path"
        hostile.mkdir()
        path_marker = self.root / "hostile-path-ran"
        function_marker = self.root / "hostile-function-ran"
        for name in ("bash", "env", "git"):
            executable = hostile / name
            executable.write_text(
                "#!/bin/sh\n/usr/bin/touch \"$HOSTILE_PATH_MARKER\"\nexit 97\n",
                encoding="utf-8",
            )
            executable.chmod(0o755)
        subprocess.run(
            ["/usr/bin/git", "init", "-q"], cwd=self.root, check=True
        )
        subprocess.run(
            [
                "/usr/bin/git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )
        self.environment["PATH"] = f"{hostile}:{self.environment.get('PATH', '')}"
        self.environment["HOSTILE_PATH_MARKER"] = str(path_marker)
        self.environment["HOSTILE_FUNCTION_MARKER"] = str(function_marker)
        for name in ("env", "git"):
            self.environment[f"BASH_FUNC_{name}%%"] = (
                "() { /usr/bin/touch \"$HOSTILE_FUNCTION_MARKER\"; return 96; }"
            )

        result = self.run_wrapper("pr", "view", "7")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(path_marker.exists())
        self.assertFalse(function_marker.exists())
        self.assertIn("--repo Generous-Corp/pulp", self.helper_log.read_text())

    def test_cli_mode_rejects_resolver_extra_keys_before_helper(self) -> None:
        self.environment["RESOLVER_MODE"] = "extra-key"

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("malformed JSON", result.stderr)
        self.assertEqual(
            result.stderr.count("malformed JSON"),
            2,
            "decoder failure must be caught again by final argv cardinality",
        )
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_cli_mode_rejects_boolean_resolver_schema_before_helper(self) -> None:
        self.environment["RESOLVER_MODE"] = "schema-bool"

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("malformed JSON", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_cli_mode_rejects_non_decimal_or_zero_resolver_app_ids(self) -> None:
        for app_id in (
            "0",
            "000",
            "not-numeric",
            "12x34",
            "１２３",
            "18446744073709551616",
        ):
            with self.subTest(app_id=app_id):
                self.environment["RESOLVER_MODE"] = f"app-id:{app_id}"
                result = self.run_wrapper(
                    "api", "repos/Generous-Corp/pulp/hooks"
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("malformed JSON", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_cli_mode_rejects_non_normalized_resolver_private_key(self) -> None:
        for private_key in (
            "/Users/ci//app.pem",
            "/Users/ci/./app.pem",
            "/Users/ci/bad\u0085key.pem",
        ):
            with self.subTest(private_key=private_key):
                self.environment["RESOLVER_MODE"] = f"private-key:{private_key}"
                result = self.run_wrapper(
                    "api", "repos/Generous-Corp/pulp/hooks"
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("malformed JSON", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_cli_mode_rejects_oversized_resolver_output_before_helper(self) -> None:
        self.environment["RESOLVER_MODE"] = "oversize"

        result = self.run_wrapper("api", "repos/Generous-Corp/pulp/hooks")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exceeds 16384 bytes", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_python_import_shadowing_cannot_capture_token_json(self) -> None:
        checkout_marker = self.root / "checkout-json-ran"
        pythonpath_root = self.root / "hostile-pythonpath"
        pythonpath_root.mkdir()
        pythonpath_marker = self.root / "pythonpath-json-ran"
        (self.root / "json.py").write_text(
            f"from pathlib import Path\nPath({str(checkout_marker)!r}).write_text('ran')\n",
            encoding="utf-8",
        )
        (pythonpath_root / "json.py").write_text(
            f"from pathlib import Path\nPath({str(pythonpath_marker)!r}).write_text('ran')\n",
            encoding="utf-8",
        )
        self.environment["PYTHONPATH"] = str(pythonpath_root)
        self.environment["PYTHONHOME"] = str(self.root / "hostile-python-home")

        result = self.run_wrapper(
            "api", "repos/Generous-Corp/pulp/hooks"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(checkout_marker.exists())
        self.assertFalse(pythonpath_marker.exists())
        self.assertEqual(self.gh_log.read_text(), "api repos/Generous-Corp/pulp/hooks\n")

    def test_cli_mode_strips_query_from_repository_endpoint(self) -> None:
        result = self.run_wrapper(
            "api",
            "repos/Generous-Corp/pulp/actions/runs?per_page=100",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Generous-Corp/pulp", self.helper_log.read_text())

    def test_absolute_cloud_api_endpoint_fails_untouched(self) -> None:
        result = self.run_wrapper(
            "api", "https://api.github.com/repos/B/target/hooks"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fully qualified API endpoints", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_absolute_untrusted_api_endpoint_fails_untouched(self) -> None:
        for endpoint in (
            "https://attacker.example/repos/B/target/hooks",
            "HTTPS://attacker.example/repos/B/target/hooks",
            "HtTpS://attacker.example/repos/B/target/hooks",
        ):
            with self.subTest(endpoint=endpoint):
                result = self.run_wrapper("api", endpoint)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("fully qualified API endpoints", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_absolute_api_endpoint_after_unmodeled_flags_fails_untouched(self) -> None:
        flag_forms = (("-iXGET",), ("--allow-escape-sequences",))
        for flags in flag_forms:
            with self.subTest(flags=flags):
                result = self.run_wrapper(
                    "api",
                    *flags,
                    "HTTPS://attacker.example/repos/B/target/hooks",
                )
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("outside privileged grammar", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_api_option_value_is_not_mistaken_for_endpoint(self) -> None:
        result = self.run_wrapper(
            "api",
            "--raw-field",
            "body=repos/other/repo",
            "repos/Generous-Corp/pulp/issues",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo Generous-Corp/pulp", helper_args)
        self.assertNotIn("--repo other/repo", helper_args)

    def test_api_placeholders_use_checkout_repository(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper("api", "repos/{owner}/{repo}/releases")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Generous-Corp/pulp", self.helper_log.read_text())

    def test_spaced_repo_flag_is_repository_provenance(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "7", "--repo", "danielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_attached_short_repo_flag_is_repository_provenance(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "7", "-Rdanielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_repo_shaped_option_value_is_not_repository_provenance(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper("pr", "create", "--title", "-Rrelease/notes")

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo Generous-Corp/pulp", helper_args)
        self.assertNotIn("--repo release/notes", helper_args)

    def test_real_repo_flag_after_option_payload_wins(self) -> None:
        result = self.run_wrapper(
            "pr",
            "create",
            "--title",
            "-Rrelease/notes",
            "--repo",
            "danielraffel/Shipyard",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo danielraffel/Shipyard", helper_args)
        self.assertNotIn("--repo release/notes", helper_args)

    def test_last_repo_flag_wins_after_release_notes_payload(self) -> None:
        result = self.run_wrapper(
            "release",
            "edit",
            "v1",
            "--notes",
            "-Rother/repo",
            "--repo",
            "danielraffel/Shipyard",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo danielraffel/Shipyard", helper_args)
        self.assertNotIn("--repo other/repo", helper_args)

    def test_untrusted_repo_host_fails_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "7", "--repo", "git.example.com/acme/widgets"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not trusted", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_untrusted_api_hostname_fails_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "api",
            "--hostname",
            "git.example.com",
            "repos/acme/widgets/hooks",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be github.com", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_generic_spaced_untrusted_hostname_fails_untouched(self) -> None:
        result = self.run_wrapper(
            "auth", "token", "--hostname", "git.example.com"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be github.com", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_generic_equals_untrusted_hostname_fails_untouched(self) -> None:
        result = self.run_wrapper(
            "auth", "token", "--hostname=git.example.com"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be github.com", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_auth_token_short_untrusted_hostname_fails_untouched(self) -> None:
        result = self.run_wrapper("auth", "token", "-h", "git.example.com")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("must be github.com", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_auth_token_attached_short_hostname_fails_untouched(self) -> None:
        for hostname_arg in ("-h=git.example.com", "-hgit.example.com"):
            with self.subTest(hostname_arg=hostname_arg):
                result = self.run_wrapper("auth", "token", hostname_arg)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("must be github.com", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_unsupported_auth_subcommands_fail_untouched(self) -> None:
        subcommands = ("login", "logout", "refresh", "setup-git", "switch")
        hostname_args = (
            ("-h", "git.example.com"),
            ("-h=git.example.com",),
            ("-hgit.example.com",),
        )
        for subcommand in subcommands:
            for hostname_args_case in hostname_args:
                with self.subTest(subcommand=subcommand, hostname_args=hostname_args_case):
                    result = self.run_wrapper(
                        "auth", subcommand, *hostname_args_case
                    )
                    self.assertNotEqual(result.returncode, 0)
                    self.assertIn("outside privileged grammar", result.stderr)
                    self.assertFalse(self.helper_log.exists())
                    self.assertFalse(self.gh_log.exists())

    def test_pull_request_url_overrides_checkout_repository(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper(
            "pr", "view", "https://github.com/danielraffel/Shipyard/pull/487"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo danielraffel/Shipyard", helper_args)
        self.assertNotIn("--repo Generous-Corp/pulp", helper_args)

    def test_uppercase_scheme_pull_request_url_routes_trusted_host(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "HTTPS://github.com/danielraffel/Shipyard/pull/487"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_mixed_scheme_untrusted_pull_request_url_fails_untouched(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "HtTpS://attacker.example/a/b/pull/7"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not trusted", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_plain_http_pr_issue_urls_fail_untouched(self) -> None:
        targets = (
            ("pr", "HTTP://github.com/a/b/pull/7"),
            ("issue", "HtTp://attacker.example/a/b/issues/7"),
        )
        for command, target in targets:
            with self.subTest(command=command, target=target):
                result = self.run_wrapper(command, "view", target)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("plain-HTTP", result.stderr)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_issue_url_is_repository_provenance(self) -> None:
        result = self.run_wrapper(
            "issue", "view", "https://github.com/Generous-Corp/pulp/issues/148"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Generous-Corp/pulp", self.helper_log.read_text())

    def test_url_and_repo_selector_conflict_fails_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "issue",
            "view",
            "https://github.com/A/one/issues/7",
            "--repo",
            "B/two",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot be combined", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_url_after_option_value_is_repository_provenance(self) -> None:
        result = self.run_wrapper(
            "pr",
            "view",
            "--json",
            "title",
            "https://github.com/danielraffel/Shipyard/pull/487",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_untrusted_pull_request_url_fails_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "https://git.example.com/acme/widgets/pull/7"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not trusted", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_untrusted_gh_host_fails_before_token_mint(self) -> None:
        self.environment["GH_HOST"] = "git.example.com"

        result = self.run_wrapper(
            "issue",
            "view",
            "https://github.com/Generous-Corp/pulp/issues/148",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not trusted", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_repo_shaped_payload_does_not_override_checkout(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper("pr", "create", "--body", "repos/other/repo")

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo Generous-Corp/pulp", helper_args)
        self.assertNotIn("--repo other/repo", helper_args)

    def test_http_url_option_payload_is_not_treated_as_target(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Owner/checkout.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper(
            "pr", "create", "--title", "x", "--body", "http://docs.example/path"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Owner/checkout", self.helper_log.read_text())

    def test_value_flag_url_payloads_do_not_override_checkout(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Owner/checkout.git",
            ],
            cwd=self.root,
            check=True,
        )
        cases = (
            ("pr", "merge", "7", "--subject", "https://github.com/other/repo/pull/9"),
            ("pr", "create", "--body", "https://github.com/other/repo/issues/9"),
        )
        for arguments in cases:
            with self.subTest(arguments=arguments):
                result = self.run_wrapper(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo Owner/checkout", self.helper_log.read_text())

    def test_close_comment_repo_flag_payload_does_not_override_checkout(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper(
            "pr", "close", "7", "--comment", "-Rrelease/notes"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo Generous-Corp/pulp", helper_args)
        self.assertNotIn("--repo release/notes", helper_args)

    def test_duplicate_target_url_is_not_repository_provenance(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper(
            "pr",
            "create",
            "--body",
            "https://github.com/other/repo/issues/456",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo Generous-Corp/pulp", helper_args)
        self.assertNotIn("--repo other/repo", helper_args)

    def test_unsupported_issue_close_fails_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "issue", "close", "123", "--repo", "danielraffel/Shipyard"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside privileged grammar", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_cli_mode_fails_closed_without_repository_provenance(self) -> None:
        result = self.run_wrapper("api", "rate_limit")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("exact repository provenance is required", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertNotIn("ghs_", result.stdout + result.stderr)

    def test_alias_and_extension_invocations_fail_untouched(self) -> None:
        commands = (
            ("dangerous-close-alias", "7"),
            ("alias", "exec", "dangerous-close-alias", "7"),
            ("extension", "exec", "dangerous-extension", "7"),
        )
        for arguments in commands:
            with self.subTest(arguments=arguments):
                result = self.run_wrapper(*arguments)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.helper_log.exists())
                self.assertFalse(self.gh_log.exists())

    def test_repo_view_slug_is_repository_provenance(self) -> None:
        result = self.run_wrapper("repo", "view", "danielraffel/Shipyard")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_repo_clone_is_outside_privileged_surface(self) -> None:
        result = self.run_wrapper(
            "repo", "clone", "--", "Generous-Corp/pulp", "destination"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside privileged grammar", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_repo_option_value_is_not_mistaken_for_positional_repository(self) -> None:
        result = self.run_wrapper(
            "repo",
            "view",
            "--branch",
            "feature/topic",
            "danielraffel/Shipyard",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo danielraffel/Shipyard", helper_args)
        self.assertNotIn("--repo feature/topic", helper_args)

    def test_repo_attached_option_value_is_not_positional_repository(self) -> None:
        result = self.run_wrapper(
            "repo",
            "view",
            "-bfeature/topic",
            "danielraffel/Shipyard",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo danielraffel/Shipyard", helper_args)
        self.assertNotIn("--repo feature/topic", helper_args)

    def test_repo_url_is_normalized_to_repository_provenance(self) -> None:
        result = self.run_wrapper(
            "repo", "view", "https://github.com/danielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_uppercase_scheme_repo_url_routes_trusted_host(self) -> None:
        result = self.run_wrapper(
            "repo", "view", "HTTPS://github.com/danielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo danielraffel/Shipyard", self.helper_log.read_text())

    def test_mixed_scheme_untrusted_repo_url_fails_untouched(self) -> None:
        result = self.run_wrapper(
            "repo", "view", "HtTpS://attacker.example/a/b"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not trusted", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_gh_repo_environment_is_repository_provenance(self) -> None:
        self.environment["GH_REPO"] = "Generous-Corp/forge"

        result = self.run_wrapper("pr", "view", "47")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Generous-Corp/forge", self.helper_log.read_text())

    def test_shipyard_route_binds_native_repo_over_checkout(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Owner/checkout.git",
            ],
            cwd=self.root,
            check=True,
        )
        self.environment["SHIPYARD_GHAPP_REPO"] = "Other/route"

        result = self.run_wrapper("pr", "close", "7")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Other/route", self.helper_log.read_text())
        self.assertEqual(
            self.gh_env_log.read_text().strip().split("|")[-1],
            "Other/route",
        )

    def test_explicit_repo_overrides_gh_repo_environment(self) -> None:
        self.environment["GH_REPO"] = "Generous-Corp/forge"

        result = self.run_wrapper(
            "repo", "view", "danielraffel/Shipyard"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        helper_args = self.helper_log.read_text()
        self.assertIn("--repo danielraffel/Shipyard", helper_args)
        self.assertNotIn("--repo Generous-Corp/forge", helper_args)

    def test_missing_repo_flag_value_fails_before_token_mint(self) -> None:
        self.environment["GH_REPO"] = "Generous-Corp/forge"

        result = self.run_wrapper("pr", "view", "47", "--repo")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--repo requires", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_later_repo_after_boolean_flag_fails_closed_before_guard(self) -> None:
        result = self.run_wrapper(
            "pr",
            "close",
            "7",
            "--repo",
            "owner/a",
            "--delete-branch",
            "--repo",
            "owner/b",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("multiple repository selectors", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_repo_selector_after_guarded_boolean_flags_is_authoritative(self) -> None:
        result = self.run_wrapper(
            "pr",
            "merge",
            "7",
            "--auto",
            "--merge",
            "--repo",
            "owner/repo",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_boolean_flag_permutations_preserve_following_repo_selector(self) -> None:
        cases = (
            ("pr", "view", "7", "--comments", "--repo", "owner/repo"),
            ("pr", "close", "7", "-d", "--repo", "owner/repo"),
            ("pr", "merge", "7", "-m", "--repo", "owner/repo"),
            ("pr", "merge", "7", "--auto", "--repo", "owner/repo"),
        )
        for arguments in cases:
            with self.subTest(arguments=arguments):
                result = self.run_wrapper(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_overloaded_short_booleans_preserve_repo_across_command_families(self) -> None:
        cases = (
            ("auth", "status", "-a", "--repo", "owner/repo"),
            ("issue", "view", "7", "-c", "--repo", "owner/repo"),
            ("pr", "create", "-f", "--repo", "owner/repo"),
            ("release", "view", "v1", "-w", "--repo", "owner/repo"),
            ("repo", "view", "-w", "--repo", "owner/repo"),
            ("run", "list", "-a", "--repo", "owner/repo"),
            ("secret", "list", "-u", "--repo", "owner/repo"),
            ("workflow", "list", "-a", "--repo", "owner/repo"),
        )
        for arguments in cases:
            with self.subTest(arguments=arguments):
                result = self.run_wrapper(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_overloaded_long_booleans_preserve_repo_across_command_families(self) -> None:
        cases = (
            ("auth", "status", "--active", "--repo", "owner/repo"),
            ("issue", "view", "7", "--comments", "--repo", "owner/repo"),
            ("pr", "create", "--fill", "--repo", "owner/repo"),
            ("release", "edit", "v1", "--draft", "--repo", "owner/repo"),
            ("repo", "view", "--web", "--repo", "owner/repo"),
            ("run", "list", "--all", "--repo", "owner/repo"),
            ("secret", "list", "--user", "--repo", "owner/repo"),
            ("workflow", "list", "--all", "--repo", "owner/repo"),
        )
        for arguments in cases:
            with self.subTest(arguments=arguments):
                result = self.run_wrapper(*arguments)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_attached_value_options_preserve_following_repo_selector(self) -> None:
        for option in ("--title=Bug", "-tBug"):
            with self.subTest(option=option):
                result = self.run_wrapper("pr", "create", option, "--repo", "owner/repo")
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_match_head_commit_attached_value_preserves_repo_selector(self) -> None:
        result = self.run_wrapper(
            "pr",
            "merge",
            "7",
            "--match-head-commit=abc123",
            "--repo",
            "owner/repo",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_attached_boolean_value_preserves_repo_selector(self) -> None:
        result = self.run_wrapper(
            "release",
            "edit",
            "v1",
            "--latest=false",
            "--repo",
            "owner/repo",
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_missing_option_value_before_repo_fails_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "pr", "create", "--title", "--repo", "owner/repo"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("--title requires a value before --repo", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_explicit_dot_git_repository_name_is_preserved(self) -> None:
        result = self.run_wrapper(
            "pr", "view", "7", "--repo", "owner/project.git"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo owner/project.git", self.helper_log.read_text())

    def test_repo_selector_after_merge_short_boolean_is_authoritative(self) -> None:
        result = self.run_wrapper(
            "pr", "merge", "7", "-m", "--repo", "owner/repo"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_pr_merge_url_after_strategy_shorthand_routes_target(self) -> None:
        for strategy in ("-m", "-r", "-s"):
            with self.subTest(strategy=strategy):
                result = self.run_wrapper(
                    "pr",
                    "merge",
                    strategy,
                    "https://github.com/owner/target/pull/7",
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("--repo owner/target", self.helper_log.read_text())

    def test_repo_selector_after_close_short_boolean_is_authoritative(self) -> None:
        result = self.run_wrapper(
            "pr", "close", "7", "-d", "--repo", "owner/repo"
        )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo owner/repo", self.helper_log.read_text())

    def test_multiple_repo_selectors_fail_before_token_mint(self) -> None:
        result = self.run_wrapper(
            "pr",
            "view",
            "7",
            "--repo",
            "owner/one",
            "--repo",
            "owner/two",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("multiple repository selectors", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_auth_token_compatibility_uses_checkout_repository(self) -> None:
        subprocess.run(
            ["git", "init", "-q"],
            cwd=self.root,
            check=True,
        )
        subprocess.run(
            [
                "git",
                "remote",
                "add",
                "origin",
                "git@github.com:Generous-Corp/pulp.git",
            ],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper("auth", "token")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo Generous-Corp/pulp", self.helper_log.read_text())
        self.assertEqual(self.gh_log.read_text(), "auth token\n")

    def test_ambiguous_checkout_remotes_fail_before_token_mint(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            ["git", "remote", "add", "origin", "git@github.com:me/fork.git"],
            cwd=self.root,
            check=True,
        )
        subprocess.run(
            ["git", "remote", "add", "upstream", "git@github.com:org/up.git"],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper("pr", "view", "7")

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("ambiguous across remotes", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_gh_default_remote_routes_checkout_repository(self) -> None:
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        subprocess.run(
            ["git", "remote", "add", "origin", "git@github.com:me/fork.git"],
            cwd=self.root,
            check=True,
        )
        subprocess.run(
            ["git", "remote", "add", "upstream", "git@github.com:org/up.git"],
            cwd=self.root,
            check=True,
        )
        subprocess.run(
            ["git", "config", "remote.upstream.gh-resolved", "base"],
            cwd=self.root,
            check=True,
        )

        result = self.run_wrapper("pr", "view", "7")

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("--repo org/up", self.helper_log.read_text())

    def test_existing_merge_guard_uses_app_token_before_native_gh(self) -> None:
        guards = self.root / "guards"
        guard = guards / "merge-guard"
        guard.write_text(
            "#!/bin/sh\n"
            "[ \"${GH_TOKEN:-}\" = ghs_private_fixture ] || exit 0\n"
            "[ \"${MERGE_GUARD_GH:-}\" = \"$GH_EXPECTED\" ] || exit 0\n"
            "exit 73\n",
            encoding="utf-8",
        )
        guard.chmod(0o755)

        result = self.run_wrapper(
            "pr", "merge", "7", "--repo", "danielraffel/Shipyard"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_inherited_repo_before_merge_subcommand_fails_before_token(self) -> None:
        result = self.run_wrapper(
            "pr", "--repo", "owner/repo", "merge", "7", "--disable-auto"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside privileged grammar", result.stderr)
        self.assertFalse(self.helper_log.exists())

    def test_pr_close_guard_uses_app_token_before_native_gh(self) -> None:
        guards = self.root / "guards"
        guard = guards / "pr-close-guard"
        guard.write_text(
            "#!/bin/sh\n"
            "[ \"${GH_TOKEN:-}\" = ghs_private_fixture ] || exit 0\n"
            "[ \"${GHAPP_REAL_GH:-}\" = \"$GH_EXPECTED\" ] || exit 0\n"
            "exit 74\n",
            encoding="utf-8",
        )
        guard.chmod(0o755)

        result = self.run_wrapper(
            "pr", "close", "7", "--repo", "danielraffel/Shipyard"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_pr_review_url_is_outside_privileged_surface_untouched(self) -> None:
        result = self.run_wrapper(
            "pr",
            "review",
            "--comment",
            "https://github.com/other/repo/pull/7",
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("outside privileged grammar", result.stderr)
        self.assertFalse(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_queue_removal_guard_uses_app_token_before_native_gh(self) -> None:
        guards = self.root / "guards"
        guard = guards / "queue-removal-guard"
        guard.write_text(
            "#!/bin/sh\n"
            "[ \"${GH_TOKEN:-}\" = ghs_private_fixture ] || exit 0\n"
            "[ \"${GHAPP_REAL_GH:-}\" = \"$GH_EXPECTED\" ] || exit 0\n"
            "exit 75\n",
            encoding="utf-8",
        )
        guard.chmod(0o755)

        result = self.run_wrapper(
            "api", "repos/Generous-Corp/pulp/hooks"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_missing_pr_close_guard_fails_before_native_gh(self) -> None:
        (self.root / "guards/pr-close-guard").unlink()

        result = self.run_wrapper(
            "pr", "close", "7", "--repo", "danielraffel/Shipyard"
        )

        self.assertNotEqual(result.returncode, 0)
        self.assertIn("required PR-close guard is unavailable", result.stderr)
        self.assertTrue(self.helper_log.exists())
        self.assertFalse(self.gh_log.exists())

    def test_mutation_capable_surfaces_dispatch_pr_close_guard(self) -> None:
        commands = (
            ("api", "repos/owner/repo/issues/7", "-XPATCH", "-fstate=closed"),
            ("pr", "close", "7", "--repo", "owner/repo"),
            ("pr", "create", "--title", "fixture", "--repo", "owner/repo"),
            ("pr", "merge", "7", "--merge", "--repo", "owner/repo"),
            ("pr", "update-branch", "7", "--repo", "owner/repo"),
            ("release", "edit", "v1", "--draft", "--repo", "owner/repo"),
            ("run", "cancel", "1", "--repo", "owner/repo"),
            ("run", "rerun", "1", "--repo", "owner/repo"),
            ("secret", "set", "NAME", "--body", "value", "--repo", "owner/repo"),
            ("variable", "set", "NAME", "--body", "value", "--repo", "owner/repo"),
            ("workflow", "run", "build.yml", "--repo", "owner/repo"),
        )
        for command in commands:
            with self.subTest(command=command):
                self.guard_log.unlink(missing_ok=True)
                result = self.run_wrapper(*command)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.guard_log.read_text().strip(), " ".join(command))


if __name__ == "__main__":
    unittest.main()
