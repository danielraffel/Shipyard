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


class FleetRolloutStageTests(unittest.TestCase):
    def config(self, **overrides: object) -> release_macos_local.ReleaseConfig:
        values: dict[str, object] = dict(
            tag="v0.209.0",
            repo="danielraffel/Shipyard",
            artifact_prefix="shipyard",
            dist_dir=Path("dist"),
            upload=True,
            ci_mode=False,
            skip_build=True,
            binary=None,
            cargo_target=None,
        )
        values.update(overrides)
        return release_macos_local.ReleaseConfig(**values)  # type: ignore[arg-type]

    def fleet_runner(
        self,
        code: int,
        events: list[dict],
        *,
        capable: int = 0,
        plan_code: int = 0,
        plan_stderr: str = "",
    ) -> FakeRunner:
        runner = FakeRunner(assets=complete_release_assets())
        runner.fleet_calls = []  # type: ignore[attr-defined]

        runner.probe_calls = []  # type: ignore[attr-defined]

        def run_status(args: list[str]) -> tuple[int, str, str]:
            if args[1:] == ["runner", "fleet-reconcile", "--help"]:
                runner.probe_calls.append(args)  # type: ignore[attr-defined]
                return capable, "", ""
            if "--apply" not in args:
                runner.probe_calls.append(args)  # type: ignore[attr-defined]
                return plan_code, "{}", plan_stderr
            runner.fleet_calls.append(args)  # type: ignore[attr-defined]
            stdout = "".join(json.dumps(event, indent=2) + "\n" for event in events)
            return code, stdout, "" if code == 0 else "fleet update stopped"

        runner.run_status = run_status  # type: ignore[method-assign]
        return runner

    def run_main(
        self, runner: FakeRunner, *argv: str, ci_mode: bool = False
    ) -> tuple[int | str | None, str, str]:
        stdout, stderr = StringIO(), StringIO()
        with (
            mock.patch.object(release_macos_local, "CommandRunner", return_value=runner),
            mock.patch.object(release_macos_local, "load_release_environment"),
            mock.patch.object(release_macos_local, "resolve_environment_files", return_value=[]),
            mock.patch.object(release_macos_local, "check_unattended_auth", return_value="api-key"),
            mock.patch.object(release_macos_local, "package_signed_dmg", return_value=Path("x.dmg")),
            mock.patch.object(release_macos_local, "upload_artifact_and_checksums"),
            mock.patch.object(release_macos_local, "publish_if_ready", return_value="published"),
            mock.patch("sys.stderr", stderr),
            redirect_stdout(stdout),
        ):
            try:
                code: int | str | None = release_macos_local.main(
                    ["--tag", "v0.209.0", "--upload", *(["--ci-mode"] if ci_mode else []), *argv]
                )
            except SystemExit as error:
                code = error.code
        return code, stdout.getvalue(), stderr.getvalue()

    def test_release_rolls_out_to_every_host_and_requires_a_verified_summary(self) -> None:
        runner = self.fleet_runner(
            0,
            [
                {"event": "host_verification", "host_class": "m1", "verdict": "verified"},
                {"event": "fleet_summary", "verdict": "verified", "verified_hosts": ["m1", "m5", "studio"]},
            ],
        )
        code, out, _ = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, 0)
        self.assertEqual(
            runner.fleet_calls,  # type: ignore[attr-defined]
            [["/opt/ctl/shipyard", "--json", "runner", "fleet-update", "--to", "v0.209.0", "--all-hosts", "--apply", "--lagging-only"]],
        )
        self.assertIn("fleet rollout verified at v0.209.0: m1, m5, studio", out)

    def test_failed_fleet_fails_the_release_loudly_and_names_lagging_hosts(self) -> None:
        runner = self.fleet_runner(
            1,
            [
                {
                    "event": "fleet_summary",
                    "verdict": "failed",
                    "verified_hosts": ["m1"],
                    "failed_host": {"host_class": "m5", "reason": "m5 failed post-rollout verification: daemon answers as version"},
                    "not_attempted_hosts": ["studio"],
                },
            ],
        )
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, release_macos_local.FLEET_ROLLOUT_FAILED_EXIT)
        self.assertIn("FLEET ROLLOUT FAILED for v0.209.0", err)
        self.assertIn("release stays published", err)
        self.assertIn("Hosts not verified: m5, studio", err)
        # The published release is never reverted to draft by the fleet stage.
        self.assertFalse(any("--draft=true" in command for command in runner.commands))

    def test_zero_exit_without_a_verified_summary_is_still_a_failure(self) -> None:
        runner = self.fleet_runner(0, [{"event": "host_result", "host_class": "m1", "ok": True}])
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, release_macos_local.FLEET_ROLLOUT_FAILED_EXIT)
        self.assertIn("UNKNOWN (no fleet summary was produced)", err)

    def test_opt_out_warns_and_skips_the_rollout(self) -> None:
        runner = self.fleet_runner(0, [])
        code, _, err = self.run_main(runner, "--no-fleet-rollout")
        self.assertEqual(code, 0)
        self.assertEqual(runner.fleet_calls, [])  # type: ignore[attr-defined]
        self.assertIn("WARNING: --no-fleet-rollout", err)
        self.assertIn("not done until every host verifies", err)

    def test_exit_zero_with_a_failed_summary_is_a_failure(self) -> None:
        runner = self.fleet_runner(
            0,
            [
                {
                    "event": "fleet_summary",
                    "verdict": "failed",
                    "verified_hosts": [],
                    "failed_host": {"host_class": "m1", "reason": "m1 failed"},
                    "not_attempted_hosts": ["studio"],
                }
            ],
        )
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, release_macos_local.FLEET_ROLLOUT_FAILED_EXIT)
        self.assertIn("Hosts not verified: m1, studio", err)

    def test_ci_mode_skips_the_rollout_with_a_warning(self) -> None:
        runner = self.fleet_runner(0, [])
        code, _, err = self.run_main(
            runner, "--fleet-shipyard", "/opt/ctl/shipyard", ci_mode=True
        )
        self.assertEqual(code, 0)
        self.assertEqual(runner.fleet_calls, [])  # type: ignore[attr-defined]
        self.assertEqual(runner.probe_calls, [])  # type: ignore[attr-defined]
        self.assertIn("WARNING: --ci-mode has no fleet", err)

    def test_controller_without_verified_rollouts_falls_back_to_the_backstop(self) -> None:
        runner = self.fleet_runner(0, [], capable=2)
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, 0)
        self.assertEqual(runner.fleet_calls, [])  # type: ignore[attr-defined]
        self.assertIn("predates verified fleet rollouts", err)

    def test_mac_without_host_classes_falls_back_to_the_backstop(self) -> None:
        runner = self.fleet_runner(
            0,
            [],
            plan_code=1,
            plan_stderr="No [host_class.<name>] configured — fleet-update has no rollout targets.",
        )
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, 0)
        self.assertEqual(runner.fleet_calls, [])  # type: ignore[attr-defined]
        self.assertIn("declares no [host_class.*]", err)

    def test_a_refused_plan_fails_the_stage(self) -> None:
        runner = self.fleet_runner(
            0, [], plan_code=1, plan_stderr="fleet release is ineligible: missing attestation"
        )
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, release_macos_local.FLEET_ROLLOUT_FAILED_EXIT)
        self.assertIn("rollout plan was refused", err)
        self.assertIn("missing attestation", err)
        self.assertEqual(runner.fleet_calls, [])  # type: ignore[attr-defined]

    def test_a_held_controller_lock_hands_over_instead_of_failing(self) -> None:
        runner = self.fleet_runner(75, [])
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, 0)
        self.assertIn("another fleet rollout holds the controller lock", err)
        self.assertNotIn("UNKNOWN (no fleet summary", err)

    def test_a_failed_rollback_is_called_out_as_needing_an_operator(self) -> None:
        runner = self.fleet_runner(
            7,
            [
                {
                    "event": "fleet_summary",
                    "verdict": "rollback_failed",
                    "verified_hosts": [],
                    "failed_host": {"host_class": "m5", "reason": "ROLLBACK TO v0.208.0 FAILED", "needs_operator": True},
                    "not_attempted_hosts": ["studio"],
                }
            ],
        )
        code, _, err = self.run_main(runner, "--fleet-shipyard", "/opt/ctl/shipyard")
        self.assertEqual(code, release_macos_local.FLEET_ROLLOUT_FAILED_EXIT)
        self.assertIn("FLEET ROLLBACK FAILED for v0.209.0: m5", err)
        self.assertIn("needs an operator now", err)

    def test_missing_controller_binary_fails_instead_of_skipping(self) -> None:
        runner = self.fleet_runner(0, [])
        with mock.patch.object(release_macos_local.shutil, "which", return_value=None):
            code, _, err = self.run_main(runner)
        self.assertEqual(code, release_macos_local.FLEET_ROLLOUT_FAILED_EXIT)
        self.assertIn("no `shipyard` controller binary", err)


if __name__ == "__main__":
    unittest.main()
