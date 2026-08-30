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
        self.assertEqual(
            rows["windows"]["companion_binary"],
            "target/release/shipyard-workstream-provider.exe",
        )
        self.assertEqual(
            rows["linux"]["companion_binary"],
            "target/release/shipyard-workstream-provider",
        )
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

    def test_sandbox_local_macos_requires_explicit_m3_canary_capability(self) -> None:
        matrix = ci_matrix.workflow_matrix(
            "sandbox-e2e", {"REQUESTED_PROVIDER": "local"}
        )
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(
            json.loads(rows["macos-arm64"]["runs_on_json"]),
            ["self-hosted", "local-mac", "shipyard-sandbox-m3"],
        )

    def test_sandbox_local_override_cannot_escape_m3_capability(self) -> None:
        matrix = ci_matrix.workflow_matrix(
            "sandbox-e2e",
            {
                "REQUESTED_PROVIDER": "local",
                "LOCAL_MACOS_ARM64_RUNS_ON_JSON": '["self-hosted","studio"]',
            },
        )
        rows = {row["key"]: row for row in matrix["include"]}
        self.assertEqual(
            json.loads(rows["macos-arm64"]["runs_on_json"]),
            ["self-hosted", "studio", "shipyard-sandbox-m3"],
        )

    def test_workflows_do_not_implicitly_route_macos_to_local_runner(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow_dir = root / ".github" / "workflows"
        for path in workflow_dir.glob("*.yml"):
            text = path.read_text(encoding="utf-8")
            self.assertNotIn("MACOS_ARM64_LOCAL_SELECTOR_JSON", text, path.name)

    def test_ci_rust_tests_do_not_inherit_runner_shipyard_configuration(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/ci.yml").read_text(encoding="utf-8")
        self.assertEqual(workflow.count("HOME: ${{ runner.temp }}"), 2)
        self.assertIn("Never let a self-hosted runner's production Shipyard", workflow)
        self.assertIn("Coverage executes the same tests", workflow)

    def test_sandbox_m3_bootstrap_is_fenced_by_target_provider_and_host(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        predicate = (
            "matrix.key == 'macos-arm64' && matrix.provider == 'local' && "
            "startsWith(runner.name, 'Shipyard-studio-')"
        )
        self.assertEqual(workflow.count(predicate), 3)
        self.assertNotIn("startsWith(runner.name, 'pulp-studio')", workflow)
        self.assertIn('test "$RUNNER_NAME" = "Shipyard-studio-02"', workflow)
        self.assertIn('test "$(hostname)" = "Daniels-Mac-Studio.local"', workflow)
        self.assertIn('test "$($installed runner tag)" = "studio"', workflow)

    def test_sandbox_m3_candidate_is_exact_and_production_is_restored(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('canary_root="/tmp/shipyard-sandbox-m3-', workflow)
        self.assertIn('"$candidate" --mode isolated', workflow)
        self.assertIn('--global-dir "$canary_root/global"', workflow)
        self.assertIn('--state-dir "$canary_root/state"', workflow)
        self.assertIn('.tunnel.backend == "inactive"', workflow)
        self.assertIn('SHIPYARD_BINARY_FOR_TEST="$candidate"', workflow)
        self.assertEqual(workflow.count("sandbox-audit-exec"), 2)
        self.assertEqual(workflow.count('--authority-sha "$GITHUB_SHA"'), 2)
        self.assertIn(
            'sandbox-e2e-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}-${RUNNER_NAME}',
            workflow,
        )
        self.assertIn('candidate_sha256:$candidate_hash', workflow)
        self.assertIn('sandbox_passed:true', workflow)
        self.assertIn('.real_home == env.HOME', workflow)
        self.assertIn('.mode == "shipyard"', workflow)
        self.assertIn('(.active_runs == [])', workflow)
        self.assertIn('"$canary_root/admission-deferred.json"', workflow)
        self.assertIn(
            'scripts/sandbox_admission_deferral.py',
            workflow,
        )
        self.assertEqual(workflow.count('ps -p "$production_pid" -o lstart='), 2)
        self.assertEqual(
            workflow.count(
                'jq -r .old_production_start_time "$canary_root/admission-deferred.json'
            ),
            2,
        )
        self.assertEqual(workflow.count('jq -r .mutation_probe_output'), 2)
        self.assertIn(
            '::notice::Sandbox E2E safely deferred because production workers are active',
            workflow,
        )
        self.assertIn('.final_production_pid', workflow)
        self.assertIn("legacy-lifetime-lock-quiesce-restore", workflow)
        self.assertIn("corrected-idle-preserve-fence", workflow)
        self.assertIn('"$canary_root/mutation-fence.json"', workflow)
        self.assertIn('returncode == 75', workflow)
        self.assertIn('overlap_classification == "sandbox_writer_domain_overlap"', workflow)
        self.assertIn('.production_identity_preserved == true', workflow)
        self.assertIn('.final_production_pid == .old_production_pid', workflow)
        self.assertIn('grep -F -- "--mode shipyard"', workflow)

    def test_sandbox_candidate_has_host_owned_cleanup_guardian(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('launchctl bootstrap "gui/$(id -u)" "$guardian_plist"', workflow)
        self.assertIn("<key>KeepAlive</key><false/>", workflow)
        self.assertIn(
            'cp "$GITHUB_WORKSPACE/scripts/sandbox_daemon_guardian.py" "$guardian"',
            workflow,
        )
        self.assertIn('<string>--owner-pid</string><string>$$</string>', workflow)
        self.assertIn('if kill -0 "$candidate_pid"', workflow)
        self.assertIn("name: Verify M3 guardian and production daemon invariants", workflow)
        self.assertEqual(workflow.count("inherited Actions orphan tracking"), 1)
        self.assertIn(
            "((.configured_repos // []) == [])", workflow
        )
        self.assertNotIn(
            '"$candidate" daemon refresh --repo Generous-Corp/pulp', workflow
        )
        self.assertNotIn('mkdir "$lease_dir"', workflow)
        self.assertIn(".production_quiesced == true", workflow)
        self.assertIn(".production_restored == true", workflow)
        self.assertIn(".production_preserved == true", workflow)
        self.assertIn(".mutation_fence_proved == true", workflow)
        self.assertIn(".production_identity_verified == true", workflow)
        self.assertIn(".lease_removed == true", workflow)
        self.assertIn('"$installed" --mode shipyard --json daemon status', workflow)
        self.assertIn('test "$current_pid" = "$production_pid"', workflow)
        self.assertIn(".configured_repos // []) | sort) == $expected", workflow)

    def test_sandbox_failure_artifacts_stay_in_explicit_temp_roots(self) -> None:
        root = Path(__file__).resolve().parents[1]
        workflow = (root / ".github/workflows/sandbox-e2e.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            "/tmp/sye2e-${{ github.run_id }}-${{ github.run_attempt }}/**",
            workflow,
        )
        self.assertIn(
            'export TMPDIR="/tmp/sye2e-${GITHUB_RUN_ID}-${GITHUB_RUN_ATTEMPT}"',
            workflow,
        )
        self.assertNotIn("/private/var/folders/**", workflow)
        self.assertNotIn("/var/folders/**", workflow)


if __name__ == "__main__":
    unittest.main()
