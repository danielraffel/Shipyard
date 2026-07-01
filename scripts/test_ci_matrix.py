#!/usr/bin/env python3
from __future__ import annotations

import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import ci_matrix


class CiMatrixTests(unittest.TestCase):
    def test_github_hosted_is_safe_default_without_repo_vars(self) -> None:
        row = ci_matrix.resolve_runs_on("linux", {})
        self.assertEqual(row["provider"], "github-hosted")
        self.assertEqual(json.loads(row["runs_on_json"]), "ubuntu-latest")

    def test_github_hosted_provider_uses_hosted_labels(self) -> None:
        row = ci_matrix.resolve_runs_on("windows", {"REQUESTED_PROVIDER": "github-hosted"})
        self.assertEqual(row["provider"], "github-hosted")
        self.assertEqual(json.loads(row["runs_on_json"]), "windows-latest")

    def test_explicit_selector_wins_over_provider_default(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64",
            {
                "REQUESTED_PROVIDER": "namespace",
                "EXPLICIT_MACOS_ARM64_RUNNER_SELECTOR_JSON": '["self-hosted","macos","arm64"]',
                "NAMESPACE_MACOS_ARM64_RUNS_ON_JSON": '"namespace-fallback"',
            },
        )
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "macos", "arm64"],
        )

    def test_namespace_repo_var_overrides_builtin_profile(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "linux",
            {
                "REQUESTED_PROVIDER": "namespace",
                "NAMESPACE_LINUX_RUNS_ON_JSON": '"namespace-profile-custom"',
            },
        )
        self.assertEqual(json.loads(row["runs_on_json"]), "namespace-profile-custom")

    def test_invalid_selector_errors_before_workflow_dispatch(self) -> None:
        with self.assertRaises(SystemExit) as ctx:
            ci_matrix.resolve_runs_on(
                "linux",
                {"EXPLICIT_LINUX_RUNNER_SELECTOR_JSON": "{nope"},
            )
        self.assertIn("not valid JSON", str(ctx.exception))

    def test_package_smoke_matrix_carries_package_metadata(self) -> None:
        matrix = ci_matrix.workflow_matrix("package-smoke", {})
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(rows["macos-arm64"]["package_args"], "--dmg --ci-mode")
        self.assertEqual(rows["windows"]["binary"], "target/release/shipyard.exe")
        self.assertEqual(rows["linux"]["package_target"], "linux-x64")

    def test_release_matrix_carries_all_release_platforms(self) -> None:
        matrix = ci_matrix.workflow_matrix("release", {})
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(
            set(rows),
            {"macos-arm64", "linux", "linux-arm64", "windows"},
        )
        self.assertEqual(rows["linux-arm64"]["package_target"], "linux-arm64")
        self.assertEqual(rows["linux-arm64"]["provider"], "github-hosted")
        self.assertEqual(
            json.loads(rows["linux-arm64"]["runs_on_json"]),
            "ubuntu-24.04-arm",
        )
        self.assertEqual(rows["macos-arm64"]["package_target"], "macos-arm64")
        self.assertNotIn("macos-x64", rows)

    def test_linux_arm64_supports_explicit_namespace_selector(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "linux-arm64",
            {
                "REQUESTED_PROVIDER": "namespace",
                "EXPLICIT_LINUX_ARM64_RUNNER_SELECTOR_JSON": '["self-hosted","linux","arm64"]',
            },
        )
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "linux", "arm64"],
        )

    def test_github_output_writes_matrix_and_single_target_outputs(self) -> None:
        with tempfile.TemporaryDirectory() as temp:
            output = Path(temp) / "github-output"
            with mock.patch.dict(os.environ, {"GITHUB_OUTPUT": str(output)}, clear=True):
                self.assertEqual(ci_matrix.main(["--workflow", "sandbox-e2e", "--github-output"]), 0)
            values = dict(
                line.split("=", 1)
                for line in output.read_text(encoding="utf-8").splitlines()
            )
        self.assertEqual(len(json.loads(values["matrix_json"])["include"]), 2)
        self.assertEqual(json.loads(values["linux_runs_on_json"]), "ubuntu-latest")
        self.assertEqual(values["linux_provider"], "github-hosted")

    def test_local_provider_routes_macos_to_self_hosted_local_mac(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64", {"REQUESTED_PROVIDER": "local"}
        )
        self.assertEqual(row["provider"], "local")
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "local-mac"],
        )

    def test_local_provider_falls_back_to_hosted_for_targets_without_local_box(
        self,
    ) -> None:
        for target_key, hosted in (
            ("linux", "ubuntu-latest"),
            ("linux-arm64", "ubuntu-24.04-arm"),
            ("windows", "windows-latest"),
        ):
            row = ci_matrix.resolve_runs_on(
                target_key, {"REQUESTED_PROVIDER": "local"}
            )
            self.assertEqual(row["provider"], "github-hosted", target_key)
            self.assertEqual(json.loads(row["runs_on_json"]), hosted, target_key)

    def test_local_repo_var_overrides_builtin_local_label(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64",
            {
                "REQUESTED_PROVIDER": "local",
                "LOCAL_MACOS_ARM64_RUNS_ON_JSON": '["self-hosted","studio"]',
            },
        )
        self.assertEqual(row["provider"], "local")
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "studio"],
        )

    def test_explicit_selector_wins_over_local_provider(self) -> None:
        row = ci_matrix.resolve_runs_on(
            "macos-arm64",
            {
                "REQUESTED_PROVIDER": "local",
                "EXPLICIT_MACOS_ARM64_RUNNER_SELECTOR_JSON": '["self-hosted","macos","arm64"]',
            },
        )
        self.assertEqual(
            json.loads(row["runs_on_json"]),
            ["self-hosted", "macos", "arm64"],
        )

    def test_release_matrix_local_provider_routes_only_macos(self) -> None:
        matrix = ci_matrix.workflow_matrix(
            "release", {"REQUESTED_PROVIDER": "local"}
        )
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(rows["macos-arm64"]["provider"], "local")
        self.assertEqual(
            json.loads(rows["macos-arm64"]["runs_on_json"]),
            ["self-hosted", "local-mac"],
        )
        self.assertEqual(rows["linux"]["provider"], "github-hosted")
        self.assertEqual(rows["linux-arm64"]["provider"], "github-hosted")
        self.assertEqual(rows["windows"]["provider"], "github-hosted")

    def test_workflows_do_not_implicitly_route_macos_to_local_runner(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow_dir = root / ".github" / "workflows"
        for path in workflow_dir.glob("*.yml"):
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("MACOS_ARM64_LOCAL_SELECTOR_JSON", text, path.name)


if __name__ == "__main__":
    unittest.main()
