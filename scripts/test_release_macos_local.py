#!/usr/bin/env python3
from __future__ import annotations

import os
import json
import tempfile
import unittest
from contextlib import redirect_stdout
from io import StringIO
from pathlib import Path
from unittest import mock

import release_macos_local


def complete_release_assets() -> list[str]:
    return list(release_macos_local.expected_release_assets("shipyard"))


class FakeRunner(release_macos_local.CommandRunner):
    def __init__(
        self,
        *,
        assets: list[str],
        draft: bool = True,
        checksum_names: list[str] | None = None,
    ) -> None:
        self.assets = assets
        self.draft = draft
        self.checksum_names = checksum_names or [
            name for name in assets if name != "checksums.sha256"
        ]
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
            names = sorted({"shipyard-linux-x64", *self.checksum_names})
            output.write_text(
                "".join(f"{'0' * 64}  {name}\n" for name in names),
                encoding="utf-8",
            )
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
            and (
                args[0].endswith("shipyard")
                or args[0].endswith("shipyard-workstream-provider")
            )
            and args[1:] == ["--version"]
        ):
            name = Path(args[0]).name
            return f"{name} 0.1.0"
        return ""


class ReleaseMacosLocalTests(unittest.TestCase):
    def test_shell_wrapper_matches_mainline_entrypoint(self) -> None:
        wrapper = release_macos_local.ROOT / "scripts" / "release-macos-local.sh"
        content = wrapper.read_text(encoding="utf-8")
        self.assertIn("release_macos_local.py", content)
        self.assertTrue(os.access(wrapper, os.X_OK))

    def test_expected_release_assets_include_every_binary_and_checksums(self) -> None:
        assets = release_macos_local.expected_release_assets("shipyard")
        self.assertIn("shipyard-macos-arm64.dmg", assets)
        self.assertIn("shipyard-workstream-provider-linux-x64", assets)
        self.assertIn("shipyard-workstream-provider-linux-arm64", assets)
        self.assertIn("shipyard-workstream-provider-windows-x64.exe", assets)
        self.assertIn("checksums.sha256", assets)

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

    def test_package_signed_dmg_forwards_both_existing_binaries(self) -> None:
        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=False,
            ci_mode=False,
            skip_build=True,
            binary=Path("/tmp/shipyard"),
            companion_binary=Path("/tmp/shipyard-workstream-provider"),
            cargo_target=None,
        )
        artifact = Path("dist/v0.127.0/shipyard-macos-arm64.dmg")

        with mock.patch.object(
            release_macos_local.package_release,
            "package",
            return_value=[artifact],
        ) as package:
            result = release_macos_local.package_signed_dmg(config)

        parsed = package.call_args.args[0]
        self.assertEqual(parsed.binary, Path("/tmp/shipyard"))
        self.assertEqual(
            parsed.companion_binary,
            Path("/tmp/shipyard-workstream-provider"),
        )
        self.assertEqual(result, artifact)

    def test_publication_never_enables_soft_dmg_mount_smoke(self) -> None:
        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=True,
            skip_build=True,
            binary=Path("/tmp/shipyard"),
            companion_binary=Path("/tmp/shipyard-workstream-provider"),
            cargo_target=None,
        )
        artifact = Path("dist/v0.127.0/shipyard-macos-arm64.dmg")

        with mock.patch.object(
            release_macos_local.package_release,
            "package",
            return_value=[artifact],
        ) as package:
            release_macos_local.package_signed_dmg(config)

        self.assertFalse(package.call_args.args[0].ci_mode)

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

    def test_ci_mode_still_requires_public_install_e2e(self) -> None:
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
        runner = FakeRunner(assets=complete_release_assets())

        with redirect_stdout(StringIO()):
            outcome = release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(outcome, "published")
        flattened = [" ".join(command) for command in runner.commands]
        self.assertTrue(any("release edit" in command for command in flattened))
        self.assertTrue(any("--draft=false" in command for command in flattened))
        self.assertTrue(any(command.startswith("curl -fsSL") for command in flattened))
        self.assertTrue(any(command.startswith("bash ") for command in flattened))

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
        runner = FailingInstallRunner(assets=complete_release_assets(), draft=True)

        with self.assertRaises(SystemExit) as ctx:
            release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(ctx.exception.code, 4)
        edits = [" ".join(command) for command in runner.commands if "edit" in command]
        self.assertIn("--draft=false", edits[0])
        self.assertIn("--draft=true", edits[-1])

    def test_already_public_release_is_redrafted_when_install_e2e_fails(self) -> None:
        class FailingInstallRunner(FakeRunner):
            def run(self, args: list[str], **kwargs: object) -> str:
                if args and args[0] == "bash":
                    raise SystemExit("install failed")
                return super().run(args, **kwargs)

        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = FailingInstallRunner(assets=complete_release_assets(), draft=False)

        with self.assertRaises(SystemExit) as ctx:
            release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(ctx.exception.code, 4)
        self.assertTrue(runner.draft)

    def test_missing_companion_asset_keeps_release_draft(self) -> None:
        assets = complete_release_assets()
        assets.remove("shipyard-workstream-provider-windows-x64.exe")
        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = FakeRunner(assets=assets, draft=True)

        with redirect_stdout(StringIO()) as stdout:
            outcome = release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(outcome, "partial")
        self.assertIn("shipyard-workstream-provider-windows-x64.exe", stdout.getvalue())
        self.assertTrue(runner.draft)

    def test_missing_companion_checksum_keeps_release_draft(self) -> None:
        assets = complete_release_assets()
        missing_name = "shipyard-workstream-provider-linux-arm64"
        checksum_names = [
            name for name in assets
            if name not in {"checksums.sha256", missing_name}
        ]
        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        runner = FakeRunner(
            assets=assets,
            draft=True,
            checksum_names=checksum_names,
        )

        with redirect_stdout(StringIO()) as stdout:
            outcome = release_macos_local.publish_if_ready(config, runner)

        self.assertEqual(outcome, "partial")
        self.assertIn(f"checksum:{missing_name}", stdout.getvalue())

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
                        {"assets": [{"name": name} for name in complete_release_assets()]}
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
            tag="v0.127.0",
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

        self.assertIn("install:v0.127.0:shipyard 0.1.0", result)
        self.assertIn("shipyard-workstream-provider 0.1.0", result)
        bash_envs = [
            env
                for command, env in zip(runner.commands, runner.envs)
            if command and command[0] == "bash"
        ]
        self.assertEqual(len(bash_envs), 1)
        self.assertEqual(bash_envs[0]["SHIPYARD_VERSION"], "v0.127.0")
        self.assertEqual(bash_envs[0]["SHIPYARD_ARTIFACT_PREFIX"], "shipyard")
        self.assertNotIn("SHIPYARD_RUST_COMPAT_NAME", bash_envs[0])

    def test_run_install_e2e_rejects_mismatched_installed_pair(self) -> None:
        class MismatchedRunner(FakeRunner):
            def run(self, args: list[str], **kwargs: object) -> str:
                if args and args[0].endswith("shipyard-workstream-provider"):
                    return "shipyard-workstream-provider 0.127.1"
                if args and args[0].endswith("shipyard") and args[1:] == ["--version"]:
                    return "shipyard 0.127.0"
                return super().run(args, **kwargs)

        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )

        with self.assertRaisesRegex(SystemExit, "version mismatch"):
            release_macos_local.run_install_e2e(
                config, MismatchedRunner(assets=[])
            )

    def test_run_install_e2e_can_upgrade_and_rollback_between_tags(self) -> None:
        config = release_macos_local.ReleaseConfig(
            tag="v0.127.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
            rollback_tag="v0.126.2",
        )
        runner = FakeRunner(assets=[])

        result = release_macos_local.run_install_e2e(config, runner)

        self.assertIn("baseline:v0.126.2:shipyard 0.1.0:provider-absent", result)
        self.assertIn("upgrade:v0.127.0:shipyard 0.1.0", result)
        self.assertIn("rollback:v0.126.2:shipyard 0.1.0:provider-absent", result)
        bash_envs = [
            env
                for command, env in zip(runner.commands, runner.envs)
            if command and command[0] == "bash"
        ]
        self.assertEqual(
            [env["SHIPYARD_VERSION"] for env in bash_envs],
            ["v0.126.2", "v0.127.0", "v0.126.2"],
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
