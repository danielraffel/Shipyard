#!/usr/bin/env python3
from __future__ import annotations

import os
import json
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock

import release_macos_local


class FakeRunner(release_macos_local.CommandRunner):
    def __init__(self, *, assets: list[str], draft: bool = True) -> None:
        self.assets = assets
        self.draft = draft
        self.commands: list[list[str]] = []
        self.envs: list[dict[str, str] | None] = []

    def run(
        self,
        args: list[str],
        *,
        capture: bool = False,
        env: dict[str, str] | None = None,
        cwd: Path = release_macos_local.ROOT,
    ) -> str:
        self.commands.append(args)
        self.envs.append(dict(env) if env is not None else None)
        if args[:4] == ["gh", "release", "view", "--repo"] and "assets" in args:
            return "\n".join(self.assets)
        if args[:4] == ["gh", "release", "view", "--repo"] and "isDraft" in args:
            return "true" if self.draft else "false"
        if args[:4] == ["gh", "release", "download", "--repo"]:
            output = Path(args[args.index("--output") + 1])
            output.write_text("old  shipyard-linux-x64\n", encoding="utf-8")
            return ""
        if args[:4] == ["gh", "release", "edit", "--repo"]:
            self.draft = "--draft=true" in args
            return ""
        if args[:2] == ["curl", "-fsSL"]:
            return json.dumps({"assets": [{"name": name} for name in self.assets]})
        if args and args[0] == "bash":
            return ""
        if (
            args
            and (args[0].endswith("shipyard") or args[0].endswith("shipyard"))
            and args[1:] == ["--version"]
        ):
            return "shipyard 0.1.0"
        return ""


class ReleaseMacosLocalTests(unittest.TestCase):
    def make_release_repo(self, root: Path, version: str = "1.2.3") -> None:
        (root / "Cargo.toml").write_text(
            f'[package]\nname = "shipyard"\nversion = "{version}"\n',
            encoding="utf-8",
        )
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(
            ["git", "config", "user.name", "Shipyard Test"], cwd=root, check=True
        )
        subprocess.run(
            ["git", "config", "user.email", "shipyard@example.invalid"],
            cwd=root,
            check=True,
        )
        subprocess.run(["git", "add", "Cargo.toml"], cwd=root, check=True)
        subprocess.run(["git", "commit", "-qm", "release source"], cwd=root, check=True)
        subprocess.run(
            ["git", "tag", "-a", f"v{version}", "-m", f"Release v{version}"],
            cwd=root,
            check=True,
        )

    def test_shell_wrapper_matches_mainline_entrypoint(self) -> None:
        wrapper = release_macos_local.ROOT / "scripts" / "release-macos-local.sh"
        content = wrapper.read_text(encoding="utf-8")
        self.assertIn("release_macos_local.py", content)
        self.assertTrue(os.access(wrapper, os.X_OK))

    def test_expected_macos_dmgs_are_arm64_only(self) -> None:
        self.assertEqual(
            release_macos_local.expected_macos_dmgs("shipyard"),
            ("shipyard-macos-arm64.dmg",),
        )
        self.assertEqual(
            release_macos_local.expected_macos_dmgs("shipyard"),
            ("shipyard-macos-arm64.dmg",),
        )

    def test_missing_env_reports_all_required_names(self) -> None:
        with mock.patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(SystemExit) as ctx:
                release_macos_local.require_env()
        message = str(ctx.exception)
        self.assertIn("SHIPYARD_NOTARIZE_APPLE_ID", message)
        self.assertIn("SHIPYARD_NOTARIZE_TEAM_ID", message)
        self.assertIn("SHIPYARD_NOTARIZE_APP_PASSWORD", message)
        self.assertIn("SHIPYARD_SIGNING_IDENTITY", message)

    def test_x64_arch_is_refused(self) -> None:
        with self.assertRaises(SystemExit) as ctx:
            release_macos_local.require_arm64("x64")
        self.assertIn("arm64", str(ctx.exception))

    def test_release_source_binds_clean_head_tree_version_and_tag_peel(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_release_repo(root)

            source = release_macos_local.verify_release_source(
                "v1.2.3", release_macos_local.CommandRunner(), source_root=root
            )
            expected_head = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=root,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()
            expected_tree = subprocess.run(
                ["git", "rev-parse", "HEAD^{tree}"],
                cwd=root,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()

        self.assertEqual(source.version, "1.2.3")
        self.assertEqual(source.tag, "v1.2.3")
        self.assertEqual(source.head, expected_head)
        self.assertEqual(source.tree, expected_tree)

    def test_release_source_refuses_dirty_checkout(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_release_repo(root)
            (root / "untracked.txt").write_text("dirty\n", encoding="utf-8")

            with self.assertRaises(SystemExit) as ctx:
                release_macos_local.verify_release_source(
                    "v1.2.3", release_macos_local.CommandRunner(), source_root=root
                )

        self.assertIn("not clean", str(ctx.exception))

    def test_release_source_refuses_tag_at_different_commit(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_release_repo(root)
            (root / "tracked.txt").write_text("new commit\n", encoding="utf-8")
            subprocess.run(["git", "add", "tracked.txt"], cwd=root, check=True)
            subprocess.run(["git", "commit", "-qm", "after tag"], cwd=root, check=True)

            with self.assertRaises(SystemExit) as ctx:
                release_macos_local.verify_release_source(
                    "v1.2.3", release_macos_local.CommandRunner(), source_root=root
                )

        self.assertIn("checkout HEAD", str(ctx.exception))

    def test_release_source_refuses_version_tag_mismatch(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            self.make_release_repo(root)

            with self.assertRaises(SystemExit) as ctx:
                release_macos_local.verify_release_source(
                    "v9.9.9", release_macos_local.CommandRunner(), source_root=root
                )

        self.assertIn("expected 'v1.2.3'", str(ctx.exception))

    def test_command_failure_redacts_bearer_token(self) -> None:
        secret = "release-token-must-not-print"
        with self.assertRaises(SystemExit) as ctx:
            release_macos_local.CommandRunner().run(
                [
                    sys.executable,
                    "-c",
                    "import sys; sys.exit(7)",
                    f"Authorization: Bearer {secret}",
                ]
            )

        self.assertNotIn(secret, str(ctx.exception))
        self.assertIn("Authorization: Bearer <redacted>", str(ctx.exception))

    def test_release_environment_file_must_be_private(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            path = Path(temp) / "release.env"
            path.write_text("APPLE_ID=dev@example.com\n", encoding="utf-8")
            path.chmod(0o644)
            with self.assertRaises(SystemExit) as ctx:
                release_macos_local.load_release_environment([path])
        self.assertIn("0600", str(ctx.exception))

    def test_local_environment_files_are_auto_discovered_as_a_pair(self) -> None:
        files = (Path("/tmp/keychain.env"), Path("/tmp/notary.env"))
        with mock.patch.object(release_macos_local, "DEFAULT_LOCAL_ENV_FILES", files), \
                mock.patch.object(Path, "is_file", return_value=True):
            resolved = release_macos_local.resolve_environment_files([])

        self.assertEqual(resolved, list(files))

    def test_explicit_environment_files_override_m5_defaults(self) -> None:
        requested = [Path("/tmp/custom.env")]
        self.assertEqual(
            release_macos_local.resolve_environment_files(requested),
            requested,
        )

    def test_check_auth_uses_api_key_mode_and_signing_probe(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            key = Path(temp) / "AuthKey_TEST.p8"
            key.write_text("private", encoding="utf-8")
            key.chmod(0o600)
            with mock.patch.dict(
                os.environ,
                {
                    "SHIPYARD_SIGNING_IDENTITY": "identity",
                    "SHIPYARD_NOTARIZE_KEY_PATH": str(key),
                    "SHIPYARD_NOTARIZE_KEY_ID": "KEY123",
                    "SHIPYARD_NOTARIZE_ISSUER_ID": "issuer-uuid",
                },
                clear=True,
            ), mock.patch.object(
                release_macos_local.package_release,
                "require_commands",
            ), mock.patch.object(
                release_macos_local.package_release,
                "prepared_signing_keychain",
            ), mock.patch.object(
                release_macos_local.package_release,
                "verify_signing_probe",
            ) as probe:
                mode = release_macos_local.check_unattended_auth()

        self.assertEqual(mode, "api-key")
        probe.assert_called_once_with()

    def test_ci_mode_publishes_but_skips_install_e2e(self) -> None:
        config = release_macos_local.ReleaseConfig(
            tag="v0.1.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=True,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = FakeRunner(assets=["shipyard-macos-arm64.dmg", "checksums.sha256"])

        with redirect_stdout(StringIO()):
            outcome = release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(outcome, "published-ci")
        flattened = [" ".join(command) for command in runner.commands]
        self.assertTrue(any("release edit" in command for command in flattened))
        self.assertTrue(any("--draft=false" in command for command in flattened))
        self.assertTrue(any(command.startswith("curl -fsSL") for command in flattened))
        self.assertFalse(any(command.startswith("bash ") for command in flattened))

    def test_publish_reverts_draft_when_install_e2e_fails(self) -> None:
        class FailingInstallRunner(FakeRunner):
            def run(self, args: list[str], **kwargs: object) -> str:
                if args and args[0] == "bash":
                    raise SystemExit("install failed")
                return super().run(args, **kwargs)

        config = release_macos_local.ReleaseConfig(
            tag="v0.1.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = FailingInstallRunner(assets=["shipyard-macos-arm64.dmg"], draft=True)

        with self.assertRaises(SystemExit) as ctx:
            release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(ctx.exception.code, 4)
        edits = [" ".join(command) for command in runner.commands if "edit" in command]
        self.assertIn("--draft=false", edits[0])
        self.assertIn("--draft=true", edits[-1])

    def test_public_release_asset_visibility_can_retry(self) -> None:
        class EventuallyVisibleRunner(FakeRunner):
            def __init__(self) -> None:
                super().__init__(assets=[])
                self.calls = 0

            def run(self, args: list[str], **kwargs: object) -> str:
                if args[:2] == ["curl", "-fsSL"]:
                    self.calls += 1
                    if self.calls == 1:
                        return json.dumps({"assets": []})
                    return json.dumps(
                        {"assets": [{"name": "shipyard-macos-arm64.dmg"}]}
                    )
                return super().run(args, **kwargs)

        config = release_macos_local.ReleaseConfig(
            tag="v0.1.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = EventuallyVisibleRunner()

        with mock.patch("release_macos_local.time.sleep"):
            release_macos_local.wait_for_public_release_assets(
                config,
                runner,
                timeout_secs=10,
                poll_secs=1,
            )

        self.assertEqual(runner.calls, 2)

    def test_release_api_curl_args_use_private_repo_token_when_present(self) -> None:
        with mock.patch.dict(os.environ, {"SHIPYARD_GITHUB_TOKEN": "token"}, clear=True):
            args = release_macos_local.release_api_curl_args("https://example.test")

        self.assertEqual(
            args,
            [
                "curl",
                "-fsSL",
                "-H",
                "Authorization: Bearer token",
                "https://example.test",
            ],
        )

    def test_run_install_e2e_installs_current_tag_by_default(self) -> None:
        config = release_macos_local.ReleaseConfig(
            tag="v0.2.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = FakeRunner(assets=[])

        result = release_macos_local.run_install_e2e(config, runner)

        self.assertIn("install:v0.2.0:shipyard 0.1.0", result)
        bash_envs = [
            env
                for command, env in zip(runner.commands, runner.envs)
            if command and command[0] == "bash"
        ]
        self.assertEqual(len(bash_envs), 1)
        self.assertEqual(bash_envs[0]["SHIPYARD_VERSION"], "v0.2.0")
        self.assertEqual(bash_envs[0]["SHIPYARD_ARTIFACT_PREFIX"], "shipyard")
        self.assertNotIn("SHIPYARD_RUST_COMPAT_NAME", bash_envs[0])

    def test_run_install_e2e_can_upgrade_and_rollback_between_tags(self) -> None:
        config = release_macos_local.ReleaseConfig(
            tag="v0.2.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
            rollback_tag="v0.1.0",
        )
        runner = FakeRunner(assets=[])

        result = release_macos_local.run_install_e2e(config, runner)

        self.assertIn("baseline:v0.1.0:shipyard 0.1.0", result)
        self.assertIn("upgrade:v0.2.0:shipyard 0.1.0", result)
        self.assertIn("rollback:v0.1.0:shipyard 0.1.0", result)
        bash_envs = [
            env
                for command, env in zip(runner.commands, runner.envs)
            if command and command[0] == "bash"
        ]
        self.assertEqual(
            [env["SHIPYARD_VERSION"] for env in bash_envs],
            ["v0.1.0", "v0.2.0", "v0.1.0"],
        )
        self.assertTrue(all("SHIPYARD_RUST_COMPAT_NAME" not in env for env in bash_envs))
        self.assertTrue(all(env["SHIPYARD_ARTIFACT_PREFIX"] == "shipyard" for env in bash_envs))

    def test_merge_release_checksum_preserves_other_artifacts(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            artifact = root / "shipyard-macos-arm64.dmg"
            artifact.write_text("new dmg", encoding="utf-8")
            config = release_macos_local.ReleaseConfig(
                tag="v0.1.0",
                repo="danielraffel/Shipyard",
                artifact_prefix="shipyard",
                dist_dir=root,
                upload=True,
                ci_mode=False,
                skip_build=True,
                binary=None,
                cargo_target=None,
            )
            runner = FakeRunner(
                assets=["checksums.sha256", "shipyard-macos-arm64.dmg"]
            )

            checksums = release_macos_local.merge_release_checksum(
                config,
                artifact,
                runner,
            )

            lines = checksums.read_text(encoding="utf-8").splitlines()
            self.assertEqual(len(lines), 2)
            self.assertTrue(any(line.endswith("  shipyard-linux-x64") for line in lines))
            self.assertTrue(any(line.endswith("  shipyard-macos-arm64.dmg") for line in lines))


if __name__ == "__main__":
    unittest.main()
